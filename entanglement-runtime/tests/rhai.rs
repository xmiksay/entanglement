//! Integration tests for the runtime-owned `rhai` script tool (#122, ADR-0046).
//!
//! The model calls `rhai`; the executor intercepts it on `ToolExec` (before the
//! generic dispatch), runs the sandboxed engine under `spawn_blocking`, and
//! resolves each host-function binding through the *same* `Allow | Ask | Deny`
//! machinery as a model-issued tool call — delegating to the real host-tool
//! registry so root containment and bounded output come for free.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry, SessionId,
    ToolCall,
};
use entanglement_runtime::host::{host_tools, ReadRawTool};
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::spawn_tool_executor;

/// Replays scripted responses in order, then plain text so the turn terminates.
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
}
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// Unique temp dir rooted per test, cleaned on drop.
struct TempDir {
    path: std::path::PathBuf,
}
impl TempDir {
    fn new(tag: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "entanglement-rhai-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawn a Holly whose scripted LLM calls `rhai` once with `script`, wired to a
/// real host-tool registry rooted at `root` and the given `profiles`. The `rhai`
/// tool call id is `t1` (so nested binding approvals use `t1:rhai:<tool>`).
fn spawn_with_rhai(script: &str, root: &std::path::Path, profiles: ProfileRegistry) -> Holly {
    let input = serde_json::json!({ "script": script }).to_string();
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: RHAI_TOOL.into(),
                input,
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    // `read_raw` mirrors main.rs's `build_config`: registered into the same
    // registry the executor/rhai bridge use, but never advertised as a
    // standalone tool (it isn't in any `tool_specs`/`cfg.tool_specs` here).
    let mut tools = host_tools(root.to_path_buf());
    tools.register(ReadRawTool::new(root.to_path_buf()));
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    holly
}

/// A single primary profile with a caller-shaped permission, advertising every
/// tool (no mask) so binding behavior is decided by permission alone.
fn one_profile(name: &str, permission: PermissionProfile) -> ProfileRegistry {
    let mut profiles = entanglement_runtime::agents::built_in_registry();
    profiles.insert(AgentProfile {
        name: name.into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission,
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    });
    profiles
}

/// Collect events for `sid` until `Done`, with a safety timeout.
async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        if ev.session() == Some(sid) {
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                break;
            }
        }
    }
    out
}

/// The `rhai` tool output for `sid`, if any.
fn rhai_output(events: &[OutEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        OutEvent::ToolOutput { tool, output, .. } if tool == RHAI_TOOL => Some(output.clone()),
        _ => None,
    })
}

async fn prompt(holly: &Holly, sid: &SessionId, agent: &str) {
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: agent.into(),
        })
        .await
        .unwrap();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
}

#[tokio::test]
async fn allow_runs_script_captures_print_and_serializes_return() {
    let dir = TempDir::new("allow");
    let holly = spawn_with_rhai(
        r#"print("hi"); 6 * 7"#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "Allow rhai must not ask for approval"
    );
    let out = rhai_output(&events).expect("expected rhai output");
    assert_eq!(
        out, "hi\n=> 42",
        "print capture + serialized return; got {out}"
    );
}

#[tokio::test]
async fn binding_edit_delegates_and_root_contains() {
    let dir = TempDir::new("edit");
    // Create a file, then read it back and return the content — exercises the
    // edit + read bindings delegating to the real registry.
    let holly = spawn_with_rhai(
        r#"edit("f.txt", "", "hello world\n"); read("f.txt")"#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    let on_disk = std::fs::read_to_string(dir.path.join("f.txt")).unwrap();
    assert_eq!(on_disk, "hello world\n", "edit binding wrote the file");
    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("hello world"),
        "read binding returned content: {out}"
    );
}

#[tokio::test]
async fn root_escape_is_refused_by_the_binding() {
    let dir = TempDir::new("escape");
    // The read binding delegates to the host tool, whose `..` guard refuses the
    // escape — the failure surfaces to the script as the tool's message.
    let holly = spawn_with_rhai(
        r#"read("../outside.txt")"#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("escapes working directory"),
        "root escape must be refused; got {out}"
    );
}

#[tokio::test]
async fn deny_binding_surfaces_as_catchable_script_error() {
    let dir = TempDir::new("deny");
    // edit is denied by the profile; the binding throws, the script catches it.
    let holly = spawn_with_rhai(
        r#"let r = ""; try { edit("f.txt", "", "x"); r = "ran" } catch(e) { r = "caught: " + e } r"#,
        &dir.path,
        one_profile(
            "denyedit",
            PermissionProfile::new(Permission::Allow).with("edit", Permission::Deny),
        ),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "denyedit").await;
    let events = collect(sub, &sid).await;

    assert!(
        !dir.path.join("f.txt").exists(),
        "denied edit must not touch the filesystem"
    );
    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("caught") && out.contains("denied"),
        "deny should throw a catchable error; got {out}"
    );
}

#[tokio::test]
async fn ask_binding_parks_then_runs_on_approve() {
    let dir = TempDir::new("ask");
    let holly = spawn_with_rhai(
        r#"edit("f.txt", "", "approved\n"); read("f.txt")"#,
        &dir.path,
        one_profile(
            "askedit",
            PermissionProfile::new(Permission::Allow).with("edit", Permission::Ask),
        ),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    prompt(&holly, &sid, "askedit").await;

    // The binding surfaces a nested ToolRequest keyed `t1:rhai:edit`.
    let mut request_id = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), watch.recv()).await {
        if let OutEvent::ToolRequest {
            request_id: rid,
            tool,
            ..
        } = &ev
        {
            assert!(tool.contains("edit"), "card labels the binding: {tool}");
            request_id = Some(rid.clone());
            break;
        }
    }
    let request_id = request_id.expect("expected a nested ToolRequest for the edit binding");
    assert_eq!(request_id, "t1:rhai:edit");

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    let on_disk = std::fs::read_to_string(dir.path.join("f.txt")).unwrap();
    assert_eq!(on_disk, "approved\n", "edit runs after approval");
    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("approved"),
        "read returns the written content: {out}"
    );
}

#[tokio::test]
async fn ask_is_resolved_once_per_function_per_run() {
    let dir = TempDir::new("ask-once");
    // Two edits in one run; the first asks, the approval covers the second.
    let holly = spawn_with_rhai(
        r#"edit("a.txt", "", "1\n"); edit("b.txt", "", "2\n"); "done""#,
        &dir.path,
        one_profile(
            "askedit",
            PermissionProfile::new(Permission::Allow).with("edit", Permission::Ask),
        ),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    prompt(&holly, &sid, "askedit").await;

    let mut request_id = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), watch.recv()).await {
        if let OutEvent::ToolRequest {
            request_id: rid, ..
        } = &ev
        {
            request_id = Some(rid.clone());
            break;
        }
    }
    let request_id = request_id.expect("expected the first edit to ask");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    let requests = events
        .iter()
        .filter(|e| matches!(e, OutEvent::ToolRequest { .. }))
        .count();
    assert_eq!(requests, 1, "approval should cover the rest of the run");
    assert_eq!(
        std::fs::read_to_string(dir.path.join("a.txt")).unwrap(),
        "1\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path.join("b.txt")).unwrap(),
        "2\n",
        "second edit runs without a fresh prompt"
    );
}

#[tokio::test]
async fn parse_json_composes_with_the_read_raw_binding() {
    let dir = TempDir::new("parse-json");
    std::fs::write(dir.path.join("cfg.json"), r#"{"count": 41, "name": "x"}"#).unwrap();
    // parse_json/to_json are pure (no bridge round-trip) but compose with the
    // read_raw binding, which does — read_raw()'s exact file content (no
    // line-number prefix, unlike read()) feeds straight into parse_json via
    // UFCS method-call syntax.
    let holly = spawn_with_rhai(
        r#"let cfg = read_raw("cfg.json").parse_json(); cfg["count"] + 1"#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert_eq!(
        out, "=> 42",
        "read_raw() -> parse_json() -> field access -> arithmetic"
    );
}

#[tokio::test]
async fn read_raw_is_graded_and_masked_as_an_alias_of_read() {
    let dir = TempDir::new("read-raw-alias");
    std::fs::write(dir.path.join("cfg.json"), r#"{"secret": true}"#).unwrap();
    // A profile that denies `read` must also block `read_raw` — otherwise a
    // script could bypass a `read` restriction through the unlabeled raw path.
    let holly = spawn_with_rhai(
        r#"let r = ""; try { read_raw("cfg.json"); r = "leaked" } catch(e) { r = "caught: " + e } r"#,
        &dir.path,
        one_profile(
            "denyread",
            PermissionProfile::new(Permission::Allow).with("read", Permission::Deny),
        ),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "denyread").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("caught") && out.contains("denied"),
        "read_raw must be denied alongside read; got {out}"
    );
}

#[tokio::test]
async fn parse_json_failure_is_catchable_without_touching_the_bridge() {
    let dir = TempDir::new("parse-json-invalid");
    // Every binding is denied — only `rhai` itself is allowed to run — proving
    // parse_json needs no Allow/Ask/Deny resolution at all, unlike the
    // host-tool bindings.
    let holly = spawn_with_rhai(
        r#"let r = ""; try { parse_json("{not json"); } catch(e) { r = "caught: " + e } r"#,
        &dir.path,
        one_profile(
            "build",
            PermissionProfile::new(Permission::Deny).with(RHAI_TOOL, Permission::Allow),
        ),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "parse_json must not go through the permission/approval path"
    );
    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("caught"),
        "invalid JSON should be catchable: {out}"
    );
}
