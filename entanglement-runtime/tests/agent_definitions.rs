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
    LlmStream, MessageRole, OutEvent, Permission, SessionId, ToolCall, ToolRegistry,
};
use entanglement_runtime::agents::load_registry;
use entanglement_runtime::tool_runner::spawn_tool_executor;

/// Point the loader's user + project dirs at temp dirs, run `load_registry`, and
/// restore the env. Serialized via a mutex because env vars are process-global.
fn load_with_dirs(
    user: Option<&std::path::Path>,
    project_root: &std::path::Path,
) -> entanglement_core::ProfileRegistry {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();
    match user {
        Some(p) => std::env::set_var("ENTANGLEMENT_AGENTS_DIR", p),
        None => std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir"),
    }
    let reg = load_registry(project_root).expect("load_registry");
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
fn malformed_project_file_aborts_load() {
    let project = tempfile::tempdir().unwrap();
    write_agent(
        &project.path().join(".entanglement").join("agents"),
        "broken.md",
        "---\nname: broken\n---\nno description field",
    );
    // The missing `description` must surface as an error, not a silent skip.
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    let result = load_registry(project.path());
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
    spawn_tool_executor(&holly, ToolRegistry::new(), profiles);

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
