//! Integration tests for file-based agent definitions (#112, ADR-0034).
//!
//! Covers discovery + precedence (project > user > built-in) via the real
//! `load_registry`, and an end-to-end spawn under a purely file-defined profile
//! (the `subagent_spawn.rs` pattern): a parent spawns a child under a project
//! agent whose permission profile was loaded from disk.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmSession,
    LlmStream, MessageRole, OutEvent, Permission, SessionId, ToolCall,
};
use std::sync::Mutex;

use entanglement_runtime::agents::load_registry;
use entanglement_runtime::system_prompt::PromptContext;
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;

/// `ENTANGLEMENT_AGENTS_DIR` is process-global; every test that sets it must
/// serialize through this lock so parallel runs don't clobber each other's dir.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Point the loader's user + project dirs at temp dirs, run `load_registry`, and
/// restore the env. Serialized via a mutex because env vars are process-global.
fn load_with_dirs(
    user: Option<&std::path::Path>,
    project_root: &std::path::Path,
) -> entanglement_core::ProfileRegistry {
    let _guard = ENV_LOCK.lock().unwrap();
    match user {
        Some(p) => std::env::set_var("ENTANGLEMENT_AGENTS_DIR", p),
        None => std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir"),
    }
    // Identity context + empty skill registry: assert the raw file bodies, not
    // composed prompts (composition is covered by `system_prompt_assembly.rs`).
    let reg = load_registry(
        project_root,
        &PromptContext::default(),
        &entanglement_runtime::skills::SkillRegistry::default(),
    )
    .expect("load_registry");
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
    reg
}

fn write_agent(dir: &std::path::Path, file: &str, contents: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(file), contents).unwrap();
}

#[test]
fn built_ins_present_without_any_files() {
    let empty = tempfile::tempdir().unwrap();
    let reg = load_with_dirs(None, empty.path());
    assert!(reg.get("build").is_some());
    assert!(reg.get("plan").is_some());
    assert!(reg.get("explore").is_some());
    // The built-in `explore` came through the loader unchanged.
    assert_eq!(
        reg.get("explore").unwrap().permission.for_tool("edit"),
        Permission::Deny
    );
}

#[test]
fn project_overrides_user_overrides_builtin() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // User replaces the built-in `build` and adds a `reviewer`.
    write_agent(
        user.path(),
        "build.md",
        "---\nname: build\ndescription: user build\npermission:\n  default: ask\n---\nuser build prompt",
    );
    write_agent(
        user.path(),
        "reviewer.md",
        "---\nname: reviewer\ndescription: user reviewer\n---\nreview things",
    );
    // Project wins over the user's `build` and adds a `deployer`.
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "build.md",
        "---\nname: build\ndescription: project build\npermission:\n  default: deny\n---\nproject build prompt",
    );
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "deployer.md",
        "---\nname: deployer\ndescription: project deployer\n---\ndeploy things",
    );

    let reg = load_with_dirs(Some(user.path()), project.path());

    // Project `build` wins (deny default), replacing user's (ask) and built-in (allow).
    let build = reg.get("build").unwrap();
    assert_eq!(build.description, "project build");
    assert_eq!(build.permission.for_tool("edit"), Permission::Deny);
    assert_eq!(build.system_prompt, "project build prompt");
    // User-only and project-only agents both survive.
    assert_eq!(reg.get("reviewer").unwrap().description, "user reviewer");
    assert_eq!(reg.get("deployer").unwrap().description, "project deployer");
    // Untouched built-ins remain.
    assert!(reg.get("explore").is_some());
}

#[test]
fn skills_preload_composes_body_into_the_agent_prompt() {
    // End-to-end (#117): a project skill on disk + a project agent that preloads
    // it. The composed system prompt carries the skill body, and preload leaves
    // the tool mask alone — `load_skill` is still advertised (not an allowlist).
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    // A project skill with a `references/` payload so path substitution runs.
    let skill_dir = root.join(".entanglement").join("skills").join("git");
    std::fs::create_dir_all(skill_dir.join("references")).unwrap();
    std::fs::write(
        skill_dir.join("references").join("guide.md"),
        "detailed guide",
    )
    .unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: git\ndescription: git helpers\n---\nSee references/guide.md before committing.",
    )
    .unwrap();
    // A project agent that preloads the skill.
    write_agent(
        &root.join(".entanglement").join("agents"),
        "coder.md",
        "---\nname: coder\ndescription: a coder\nskills: [git]\n---\nBe careful.",
    );

    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    std::env::set_var("ENTANGLEMENT_SKILLS_DIR", "/nonexistent-user-skills-dir");
    let skills = entanglement_runtime::skills::load_registry(root).expect("load skills");
    let ctx = PromptContext {
        skills: skills.disclosures(),
        ..Default::default()
    };
    let reg = load_registry(root, &ctx, &skills).expect("load agents");
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
    std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");

    let coder = reg.get("coder").expect("coder agent");
    let prompt = &coder.system_prompt;
    assert!(prompt.contains("Preloaded skills"), "{prompt}");
    assert!(prompt.contains("skill_id: git"), "{prompt}");
    // The relative ref was substituted to an absolute path (load_skill pipeline).
    let abs_ref = skill_dir.join("references").join("guide.md");
    assert!(
        prompt.contains(&abs_ref.display().to_string()),
        "ref not absolutized:\n{prompt}"
    );
    // Preload is not an allowlist: runtime access is untouched.
    assert!(coder.advertises_tool("load_skill"));
}

#[test]
fn malformed_project_file_aborts_load() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "broken.md",
        "---\nname: broken\n---\nno description field",
    );
    // The missing `description` must surface as an error, not a silent skip.
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    let result = load_registry(
        project.path(),
        &PromptContext::default(),
        &entanglement_runtime::skills::SkillRegistry::default(),
    );
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
    let err = result.err().expect("malformed file must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("broken.md"), "error names the file: {msg}");
}

// ── end-to-end spawn under a file-defined profile ──────────────────────────────

fn finish(text: &str) -> LlmStream {
    stream_from_response(LlmResponse {
        text: text.into(),
        tool_calls: vec![],
    })
}

fn call(id: &str, name: &str, input: String) -> LlmStream {
    stream_from_response(LlmResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }],
    })
}

fn last_tool<'a>(req: &'a LlmRequest<'_>) -> Option<&'a str> {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Tool)
        .map(|m| m.text.as_str())
}

fn last_user<'a>(req: &'a LlmRequest<'_>) -> &'a str {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .map(|m| m.text.as_str())
        .unwrap_or("")
}

/// The parent delegates to a file-defined `worker` via the blocking `agent`
/// tool; the child (its system prompt is the file body) answers directly.
struct DelegateLlm;

#[async_trait]
impl Llm for DelegateLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        if last_user(&req) == "child-task" && last_tool(&req).is_none() {
            return Ok(finish("worker-answer"));
        }
        match last_tool(&req) {
            Some(_) => Ok(finish("parent done")),
            None => Ok(call(
                "a1",
                "agent",
                r#"{"agent":"worker","prompt":"child-task"}"#.into(),
            )),
        }
    }
}

#[tokio::test]
async fn spawn_under_a_file_defined_profile() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "worker.md",
        "---\nname: worker\ndescription: file-defined worker\nmode: subagent\n\
         permission:\n  default: allow\n---\nYou are the worker.",
    );
    let profiles = load_with_dirs(None, project.path());
    assert!(profiles.get("worker").is_some(), "worker loaded from disk");

    let cfg = EngineConfig {
        llm_factory: Arc::new(|| LlmSession::new(Box::new(DelegateLlm))),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: parent.clone(),
            text: "delegate".into(),
        })
        .await
        .unwrap();

    let mut child_under_worker = false;
    let mut got_answer = false;
    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p),
                profile,
                root: false,
                ..
            } if p == &parent && profile == "worker" => child_under_worker = true,
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &parent && tool == "agent" && output.contains("worker-answer") => {
                got_answer = true;
            }
            OutEvent::Done { session, .. } if session == &parent && got_answer => {
                parent_finished = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        child_under_worker,
        "the child should start under the file-defined `worker` profile"
    );
    assert!(
        got_answer,
        "the blocking `agent` returns the child's answer"
    );
    assert!(parent_finished, "the parent finishes after delegating");
}

// ---------------------------------------------------------------------------
// `inspect prompt` support: prompt_report (#184)
// ---------------------------------------------------------------------------

use entanglement_runtime::agents::prompt_report;

/// Resolve `agent` via `prompt_report` under a temp project root, isolating from
/// any host user-agents dir. Serialized on `ENV_LOCK` like `load_with_dirs`.
fn report_for(
    project_root: &std::path::Path,
    agent: &str,
    ctx: &PromptContext,
) -> Option<entanglement_runtime::agents::AgentPromptReport> {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    let report = prompt_report(
        project_root,
        agent,
        ctx,
        &entanglement_runtime::skills::SkillRegistry::default(),
    )
    .expect("prompt_report");
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
    report
}

#[test]
fn prompt_report_reports_builtin_source_and_prompt() {
    let empty = tempfile::tempdir().unwrap();
    let ctx = PromptContext::load(empty.path());
    let report = report_for(empty.path(), "build", &ctx).expect("build resolves");
    assert_eq!(report.source, "built-in (build.md)");
    // The report's prompt matches the registry-assembled one for the same inputs.
    // `load_registry` reads the process-global `ENTANGLEMENT_AGENTS_DIR`, so it
    // must run under `ENV_LOCK` with the user dir isolated — exactly as
    // `report_for` does — or a parallel test's temp user-agents dir can leak in.
    let reg = {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
        let reg = load_registry(
            empty.path(),
            &ctx,
            &entanglement_runtime::skills::SkillRegistry::default(),
        )
        .expect("load_registry");
        std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
        reg
    };
    assert_eq!(
        report.profile.system_prompt,
        reg.get("build").unwrap().system_prompt
    );
    // A primary agent gets the env block; the body part points at the winning file.
    assert!(report.parts.iter().any(|p| p.label == "environment"));
    let body = report
        .parts
        .iter()
        .find(|p| p.label == "agent body")
        .expect("body part");
    assert_eq!(body.source, "built-in (build.md)");
}

#[test]
fn prompt_report_unknown_agent_is_none() {
    let empty = tempfile::tempdir().unwrap();
    let ctx = PromptContext::load(empty.path());
    assert!(report_for(empty.path(), "does-not-exist", &ctx).is_none());
}

#[test]
fn prompt_report_prefers_project_definition() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "build.md",
        "---\nname: build\ndescription: project override\n---\nProject build body.",
    );
    let ctx = PromptContext::load(project.path());
    let report = report_for(project.path(), "build", &ctx).expect("build resolves");
    // The project file wins over the embedded built-in (later layer).
    assert!(report.source.ends_with("build.md"));
    assert!(report.source.contains(".entanglement"));
    assert!(report.profile.system_prompt.contains("Project build body."));
}

#[test]
fn prompt_report_subagent_omits_env_and_skill_index() {
    let empty = tempfile::tempdir().unwrap();
    let mut ctx = PromptContext::load(empty.path());
    ctx.skills = vec![entanglement_runtime::system_prompt::SkillDisclosure {
        name: "git".into(),
        description: "commit helpers".into(),
    }];
    // `explore` is the reference subagent: no env block, no tier-1 skill index.
    let report = report_for(empty.path(), "explore", &ctx).expect("explore resolves");
    assert!(!report.parts.iter().any(|p| p.label == "environment"));
    assert!(!report.parts.iter().any(|p| p.label == "skill index"));
}

// ---------------------------------------------------------------------------
// `inspect agents` support: resolve_registry provenance (#185)
// ---------------------------------------------------------------------------

use entanglement_runtime::agents::{resolve_registry, AgentLayer};

/// Resolve the full registry with provenance under temp user + project dirs,
/// isolating from the host user-agents dir. Serialized on `ENV_LOCK`.
fn resolve_with_dirs(
    user: Option<&std::path::Path>,
    project_root: &std::path::Path,
) -> Vec<entanglement_runtime::agents::AgentResolution> {
    let _guard = ENV_LOCK.lock().unwrap();
    match user {
        Some(p) => std::env::set_var("ENTANGLEMENT_AGENTS_DIR", p),
        None => std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir"),
    }
    let resolved = resolve_registry(
        project_root,
        &PromptContext::default(),
        &entanglement_runtime::skills::SkillRegistry::default(),
    )
    .expect("resolve_registry");
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
    resolved
}

#[test]
fn resolve_registry_reports_builtin_layer_and_no_shadow() {
    let empty = tempfile::tempdir().unwrap();
    let resolved = resolve_with_dirs(None, empty.path());
    let build = resolved
        .iter()
        .find(|r| r.profile.name == "build")
        .expect("build present");
    assert_eq!(build.layer, AgentLayer::BuiltIn);
    assert_eq!(build.source, "built-in (build.md)");
    assert!(build.shadowed.is_empty());
    // Sorted by name for a stable table.
    let names: Vec<&str> = resolved.iter().map(|r| r.profile.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
}

#[test]
fn resolve_registry_tracks_project_over_user_over_builtin() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_agent(
        user.path(),
        "build.md",
        "---\nname: build\ndescription: user build\n---\nuser body",
    );
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "build.md",
        "---\nname: build\ndescription: project build\ntools: [read, edit]\n---\nproject body",
    );

    let resolved = resolve_with_dirs(Some(user.path()), project.path());
    let build = resolved
        .iter()
        .find(|r| r.profile.name == "build")
        .expect("build present");

    // Project wins; the resolved mask/source reflect the winner.
    assert_eq!(build.layer, AgentLayer::Project);
    assert!(build.source.ends_with("build.md"));
    assert!(build.source.contains(".entanglement"));
    assert_eq!(
        build.profile.tools.as_deref(),
        Some(&["read".to_string(), "edit".to_string()][..])
    );

    // Both shadowed layers are recorded in precedence order: built-in, then user.
    let layers: Vec<AgentLayer> = build.shadowed.iter().map(|(l, _)| *l).collect();
    assert_eq!(layers, vec![AgentLayer::BuiltIn, AgentLayer::User]);
    assert_eq!(build.shadowed[0].1, "built-in (build.md)");
    assert!(build.shadowed[1].1.ends_with("build.md"));
}
