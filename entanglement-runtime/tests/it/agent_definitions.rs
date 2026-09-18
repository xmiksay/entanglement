//! Integration tests for file-based agent definitions (#112, ADR-0034).
//!
//! Covers discovery + precedence (project > user > built-in) via the real
//! `load_registry`, and an end-to-end spawn under a purely file-defined profile
//! (the `subagent_spawn.rs` pattern): a parent spawns a child under a project
//! agent loaded from disk.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    MessageRole, OutEvent, SessionId, ToolCall,
};

use entanglement_runtime::agents::load_registry;
use entanglement_runtime::mcp::McpCapabilityIndex;
use entanglement_runtime::system_prompt::PromptContext;
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;

// `ENTANGLEMENT_AGENTS_DIR` is process-global; every test that sets it must
// serialize through this lock so parallel runs don't clobber each other's dir.
// Serialized via the harness-wide `crate::env_lock()` (ADR-0180).

/// Point the loader's user + project dirs at temp dirs, run `load_registry`, and
/// restore the env. Serialized via a mutex because env vars are process-global.
fn load_with_dirs(
    user: Option<&std::path::Path>,
    project_root: &std::path::Path,
) -> entanglement_core::AgentCatalog {
    let _guard = crate::env_lock();
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
        &McpCapabilityIndex::new(),
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
    // ADR-0207 stage 6a collapsed the five-persona roster to three.
    assert!(reg.get("general").is_some());
    assert!(reg.get("plan").is_some());
    assert!(reg.get("debug").is_some());
    assert!(reg.get("explore").is_none());
    assert!(reg.get("research").is_none());
}

#[test]
fn user_layer_shadows_built_in_plan() {
    // The user layer is the intended tweak path for an embedded profile — a
    // same-name file replaces the whole definition.
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_agent(
        user.path(),
        "plan.md",
        "---\nname: plan\ndescription: user plan\n---\nuser plan prompt",
    );

    let reg = load_with_dirs(Some(user.path()), project.path());
    let plan = reg.get("plan").unwrap();
    assert_eq!(plan.description, "user plan");
    assert_eq!(plan.system_prompt, "user plan prompt");
    // And without the file, the embedded definition is what loads.
    let reg = load_with_dirs(None, project.path());
    assert!(reg.get("plan").is_some());
}

#[test]
fn project_overrides_user_overrides_builtin() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // User replaces the built-in `general` and adds a `reviewer`.
    write_agent(
        user.path(),
        "general.md",
        "---\nname: general\ndescription: user general\n---\nuser general prompt",
    );
    write_agent(
        user.path(),
        "reviewer.md",
        "---\nname: reviewer\ndescription: user reviewer\n---\nreview things",
    );
    // Project wins over the user's `general` and adds a `deployer`.
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "general.md",
        "---\nname: general\ndescription: project general\n---\nproject general prompt",
    );
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "deployer.md",
        "---\nname: deployer\ndescription: project deployer\n---\ndeploy things",
    );

    let reg = load_with_dirs(Some(user.path()), project.path());

    // Project `general` wins, replacing user's and built-in's.
    let general = reg.get("general").unwrap();
    assert_eq!(general.description, "project general");
    assert_eq!(general.system_prompt, "project general prompt");
    // User-only and project-only agents both survive.
    assert_eq!(reg.get("reviewer").unwrap().description, "user reviewer");
    assert_eq!(reg.get("deployer").unwrap().description, "project deployer");
    // Untouched built-ins remain.
    assert!(reg.get("debug").is_some());
}

#[test]
fn skills_preload_composes_body_into_the_agent_prompt() {
    // End-to-end (#117): a project skill on disk + a project agent that preloads
    // it. The composed system prompt carries the skill body; preload is not an
    // allowlist, so it has no bearing on `load_skill` access (a runtime
    // permission-mode fact, ADR-0207).
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

    let _guard = crate::env_lock();
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    std::env::set_var("ENTANGLEMENT_SKILLS_DIR", "/nonexistent-user-skills-dir");
    let skills = entanglement_runtime::skills::load_registry(root).expect("load skills");
    let ctx = PromptContext {
        skills: skills.disclosures(),
        ..Default::default()
    };
    let reg = load_registry(root, &ctx, &skills, &McpCapabilityIndex::new()).expect("load agents");
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
    let _guard = crate::env_lock();
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    let result = load_registry(
        project.path(),
        &PromptContext::default(),
        &entanglement_runtime::skills::SkillRegistry::default(),
        &McpCapabilityIndex::new(),
    );
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
    let err = result.err().expect("malformed file must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("broken.md"), "error names the file: {msg}");
}

#[test]
fn foreign_claude_agents_load_leniently_and_native_wins() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    // A Claude-Code-shaped agent: string `tools`, unknown `model`/`color` keys.
    write_agent(
        &root.join(".claude").join("agents"),
        "backend.md",
        "---\nname: backend\ndescription: claude backend\ntools: Read, Grep\nmodel: sonnet\ncolor: blue\n---\nbackend prompt",
    );
    // A malformed foreign sibling must be skipped, not abort the load.
    write_agent(
        &root.join(".claude").join("agents"),
        "broken.md",
        "---\nname: broken\n---\nno description",
    );
    // A same-name native project definition wins over the foreign one.
    write_agent(
        &root.join(".agents").join("agents"),
        "dup.md",
        "---\nname: dup\ndescription: from .agents\n---\na",
    );
    write_agent(
        &root.join(".entanglement").join("agents"),
        "dup.md",
        "---\nname: dup\ndescription: native dup\n---\nn",
    );

    let reg = load_with_dirs(None, root);

    let backend = reg.get("backend").expect("foreign agent discovered");
    assert_eq!(backend.description, "claude backend");
    assert_eq!(backend.system_prompt, "backend prompt");
    assert!(reg.get("broken").is_none(), "malformed foreign skipped");
    assert_eq!(reg.get("dup").unwrap().description, "native dup");
}

#[test]
fn prompt_report_tolerates_broken_foreign_file() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    write_agent(
        &root.join(".claude").join("agents"),
        "broken.md",
        "not even frontmatter",
    );

    let _guard = crate::env_lock();
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    let report = entanglement_runtime::agents::prompt_report(
        root,
        "general",
        &PromptContext::default(),
        &entanglement_runtime::skills::SkillRegistry::default(),
    );
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");

    let report = report.expect("broken foreign file must not abort");
    assert!(report.is_some(), "built-in general still resolves");
}

// ── per-agent provider/model pin frontmatter (#323, ADR-0081) ──────────────────

#[test]
fn provider_and_model_frontmatter_forms_a_model_pin() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "planner.md",
        "---\nname: planner\ndescription: pinned planner\nprovider: zai\nmodel: glm-5.2\n---\nbody",
    );
    let reg = load_with_dirs(None, project.path());
    let planner = reg.get("planner").expect("planner loaded");
    assert_eq!(planner.provider.as_deref(), Some("zai"));
    assert_eq!(planner.model.as_deref(), Some("glm-5.2"));
    assert_eq!(planner.model_pin(), Some(("zai", "glm-5.2")));
}

#[test]
fn model_only_frontmatter_is_not_a_pin() {
    // `model:` without `provider:` keeps the legacy request-level fallback — no
    // pin, so no rebind.
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "legacy.md",
        "---\nname: legacy\ndescription: legacy model-only\nmodel: glm-5.2\n---\nbody",
    );
    let reg = load_with_dirs(None, project.path());
    let legacy = reg.get("legacy").expect("legacy loaded");
    assert_eq!(legacy.model.as_deref(), Some("glm-5.2"));
    assert_eq!(legacy.provider, None);
    assert_eq!(legacy.model_pin(), None);
}

#[test]
fn provider_without_model_is_a_loud_error() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "broken.md",
        "---\nname: broken\ndescription: provider only\nprovider: zai\n---\nbody",
    );
    let _guard = crate::env_lock();
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    let result = load_registry(
        project.path(),
        &PromptContext::default(),
        &entanglement_runtime::skills::SkillRegistry::default(),
        &McpCapabilityIndex::new(),
    );
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
    let err = result.err().expect("provider-without-model must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("provider") && msg.contains("model"),
        "error explains the missing model: {msg}"
    );
}

#[test]
fn provider_inherit_is_treated_as_no_pin() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "inh.md",
        "---\nname: inh\ndescription: inherit\nprovider: inherit\nmodel: inherit\n---\nbody",
    );
    let reg = load_with_dirs(None, project.path());
    let inh = reg.get("inh").expect("inh loaded");
    assert_eq!(inh.provider, None);
    assert_eq!(inh.model, None);
    assert_eq!(inh.model_pin(), None);
}

#[test]
fn foreign_agents_carry_no_provider_pin() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".claude").join("agents"),
        "helper.md",
        "---\nname: helper\ndescription: claude helper\nmodel: sonnet\n---\nbody",
    );
    let reg = load_with_dirs(None, project.path());
    let helper = reg.get("helper").expect("foreign agent discovered");
    // Foreign frontmatter ignores `model`; a provider pin is never inferred.
    assert_eq!(helper.provider, None);
    assert_eq!(helper.model_pin(), None);
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
            provider_meta: None,
        }],
    })
}

fn last_tool<'a>(req: &'a LlmRequest<'_>) -> Option<&'a str> {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Tool)
        .and_then(|m| m.content.iter().find_map(|p| p.as_text()))
}

fn last_user<'a>(req: &'a LlmRequest<'_>) -> &'a str {
    req.messages
        .iter()
        .rev()
        // Skip the trailing mode notice (ADR-0207 §9) — appended fresh to
        // every request from `Session::mode`, never part of the real
        // conversation, so it must never be mistaken for what the user
        // actually said.
        .find(|m| m.role == MessageRole::User && !m.text().starts_with("[mode: "))
        .and_then(|m| m.content.iter().find_map(|p| p.as_text()))
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
        "---\nname: worker\ndescription: file-defined worker\n---\nYou are the worker.",
    );
    let profiles = load_with_dirs(None, project.path());
    assert!(profiles.get("worker").is_some(), "worker loaded from disk");

    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(DelegateLlm) as Box<dyn Llm>),
        agents: profiles.clone(),
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
        .send(InMsg::prompt(parent.clone(), "delegate"))
        .await
        .unwrap();

    let mut child_under_worker = false;
    let mut got_answer = false;
    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p),
                agent,
                root: false,
                ..
            } if p == &parent && agent == "worker" => child_under_worker = true,
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
    let _guard = crate::env_lock();
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
    let report = report_for(empty.path(), "general", &ctx).expect("general resolves");
    assert_eq!(report.source, "built-in (general.md)");
    // The report's prompt matches the registry-assembled one for the same inputs.
    // `load_registry` reads the process-global `ENTANGLEMENT_AGENTS_DIR`, so it
    // must run under `ENV_LOCK` with the user dir isolated — exactly as
    // `report_for` does — or a parallel test's temp user-agents dir can leak in.
    let reg = {
        let _guard = crate::env_lock();
        std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
        let reg = load_registry(
            empty.path(),
            &ctx,
            &entanglement_runtime::skills::SkillRegistry::default(),
            &McpCapabilityIndex::new(),
        )
        .expect("load_registry");
        std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");
        reg
    };
    assert_eq!(
        report.agent.system_prompt,
        reg.get("general").unwrap().system_prompt
    );
    // A primary agent gets the env block; the body part points at the winning file.
    assert!(report.parts.iter().any(|p| p.label == "environment"));
    let body = report
        .parts
        .iter()
        .find(|p| p.label == "agent body")
        .expect("body part");
    assert_eq!(body.source, "built-in (general.md)");
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
        "general.md",
        "---\nname: general\ndescription: project override\n---\nProject general body.",
    );
    let ctx = PromptContext::load(project.path());
    let report = report_for(project.path(), "general", &ctx).expect("general resolves");
    // The project file wins over the embedded built-in (later layer).
    assert!(report.source.ends_with("general.md"));
    assert!(report.source.contains(".entanglement"));
    assert!(report.agent.system_prompt.contains("Project general body."));
}

#[test]
fn prompt_report_includes_env_and_skill_index_for_every_agent() {
    // ADR-0207 §4 retires the old `Subagent`-mode reduced form: any agent
    // may be a session root or a spawn target, so composition no longer
    // varies — `debug` (the old reference "leaf") gets the env block and
    // tier-1 skill index too now.
    let empty = tempfile::tempdir().unwrap();
    let mut ctx = PromptContext::load(empty.path());
    ctx.skills = vec![entanglement_runtime::system_prompt::SkillDisclosure {
        name: "git".into(),
        description: "commit helpers".into(),
    }];
    let report = report_for(empty.path(), "debug", &ctx).expect("debug resolves");
    assert!(report.parts.iter().any(|p| p.label == "environment"));
    assert!(report.parts.iter().any(|p| p.label == "skill index"));
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
    let _guard = crate::env_lock();
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
    let general = resolved
        .iter()
        .find(|r| r.agent.name == "general")
        .expect("general present");
    assert_eq!(general.layer, AgentLayer::BuiltIn);
    assert_eq!(general.source, "built-in (general.md)");
    assert!(general.shadowed.is_empty());
    // Sorted by name for a stable table.
    let names: Vec<&str> = resolved.iter().map(|r| r.agent.name.as_str()).collect();
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
        "general.md",
        "---\nname: general\ndescription: user general\n---\nuser body",
    );
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "general.md",
        "---\nname: general\ndescription: project general\n---\nproject body",
    );

    let resolved = resolve_with_dirs(Some(user.path()), project.path());
    let general = resolved
        .iter()
        .find(|r| r.agent.name == "general")
        .expect("general present");

    // Project wins; the resolved layer/source reflect the winner.
    assert_eq!(general.layer, AgentLayer::Project);
    assert!(general.source.ends_with("general.md"));
    assert!(general.source.contains(".entanglement"));
    assert_eq!(general.agent.description, "project general");

    // Both shadowed layers are recorded in precedence order: built-in, then user.
    let layers: Vec<AgentLayer> = general.shadowed.iter().map(|(l, _)| *l).collect();
    assert_eq!(layers, vec![AgentLayer::BuiltIn, AgentLayer::User]);
    assert_eq!(general.shadowed[0].1, "built-in (general.md)");
    assert!(general.shadowed[1].1.ends_with("general.md"));
}
