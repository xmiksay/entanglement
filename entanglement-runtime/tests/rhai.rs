//! Integration tests for the runtime-owned `rhai` script tool (#122, ADR-0046).
//!
//! The model calls `rhai`; the executor intercepts it on `ToolExec` (before the
//! generic dispatch), runs the sandboxed engine under `spawn_blocking`, and
//! resolves each host-function binding through the *same* `Allow | Ask | Deny`
//! machinery as a model-issued tool call — delegating to the real host-tool
//! registry so root containment and bounded output come for free.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, ApprovalScope, EngineConfig, Holly, InMsg, Llm,
    LlmRequest, LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry,
    SessionId, ToolCall,
};
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::hooks::Hooks;
use entanglement_runtime::host::{
    host_tools, host_tools_with_extra_roots, BashTool, CallTool, ReadRawTool,
};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver,
};
use entanglement_runtime::skills::{load_registry, LoadSkillTool, SkillRegistry};
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::{
    spawn_tool_executor, spawn_tool_executor_with_policy, EscapeRoot,
};

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

/// [`spawn_with_rhai`] plus the exec pair registered into the same registry —
/// `call` always, `bash` only when `bash_enabled` (mirrors `main.rs`'s
/// `ENTANGLEMENT_ENABLE_BASH` gate) — so the script-facing `exec(...)`/
/// `bash(...)` bindings have a real host tool to delegate to (#419).
fn spawn_with_rhai_exec(
    script: &str,
    root: &std::path::Path,
    profiles: ProfileRegistry,
    bash_enabled: bool,
) -> Holly {
    spawn_with_rhai_exec_and_base(
        script,
        root,
        profiles,
        bash_enabled,
        PermissionProfile::new(Permission::Allow),
    )
}

/// [`spawn_with_rhai_exec`] with a caller-chosen config-level permission
/// ceiling (#172) instead of the hardcoded allow-all `base` — lets a test
/// exercise a `tool{pattern}`-style ceiling rule (#425/#480) against the
/// `rhai` `exec`/`bash` bindings.
fn spawn_with_rhai_exec_and_base(
    script: &str,
    root: &std::path::Path,
    profiles: ProfileRegistry,
    bash_enabled: bool,
    base: PermissionProfile,
) -> Holly {
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
    let mut tools = host_tools(root.to_path_buf());
    tools.register(ReadRawTool::new(root.to_path_buf()));
    tools.register(CallTool::new(root.to_path_buf()));
    if bash_enabled {
        tools.register(BashTool::new(root.to_path_buf()));
    }
    let _executor = spawn_tool_executor(&holly, tools, profiles, base);
    holly
}

/// [`spawn_with_rhai`], but wires the escape-root policy (ADR-0109) into both
/// the host-tool registry and the executor (mirroring `main.rs`'s wiring, #446)
/// instead of `spawn_tool_executor`'s no-escape-policy default — so a binding
/// targeting a path outside `root` is gated by the approval-and-record flow
/// instead of a hard containment refusal. Returns the `Holly` plus the
/// in-memory [`ExtraRootStore`] so a test can pre-seed or inspect grants.
fn spawn_with_rhai_escape(
    script: &str,
    root: &std::path::Path,
    profiles: ProfileRegistry,
) -> (Holly, Arc<ExtraRootStore>) {
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
    let store = Arc::new(ExtraRootStore::ephemeral());
    let mut tools = host_tools_with_extra_roots(root.to_path_buf(), Some(store.clone()));
    tools.register(ReadRawTool::new(root.to_path_buf()));
    let base = PermissionProfile::new(Permission::Allow);
    let active = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        active.clone(),
        base.clone(),
        Some(root.to_path_buf()),
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let escape_root = EscapeRoot {
        root: root.to_path_buf(),
        store: store.clone(),
    };
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        tools.shared(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        base,
        active,
        resolver,
        grants,
        Hooks::default(),
        Some(escape_root),
    );
    (holly, store)
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

/// Like [`collect`], but auto-approves every `ToolRequest` for `sid` the
/// instant it arrives, so a script that issues more than one `Ask`-graded
/// binding call in sequence can run to completion without the test having to
/// pre-know how many approvals are coming.
async fn collect_auto_approving(
    holly: &Holly,
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        if ev.session() != Some(sid) {
            continue;
        }
        if let OutEvent::ToolRequest { request_id, .. } = &ev {
            holly
                .send(InMsg::Approve {
                    session: sid.clone(),
                    request_id: request_id.clone(),
                    scope: Default::default(),
                })
                .await
                .unwrap();
        }
        let done = matches!(ev, OutEvent::Done { .. });
        out.push(ev);
        if done {
            break;
        }
    }
    out
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

/// #446: with the escape-root policy wired (unlike the test above), a binding
/// targeting an out-of-root path is no longer a hard refusal — it forces an
/// approval carrying the ADR-0109 warning, and on approval both runs the call
/// and durably records the grant.
#[tokio::test]
async fn escape_root_wired_prompts_with_warning_and_runs_on_approve() {
    let dir = TempDir::new("escape-approve");
    let outside = TempDir::new("escape-approve-target");
    let outside_name = outside.path.file_name().unwrap().to_str().unwrap();
    let rel = format!("../{outside_name}/f.txt");
    let script = format!(r#"write("{rel}", "hello world\n")"#);

    let (holly, store) = spawn_with_rhai_escape(
        &script,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;

    let mut saw_warning = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("subscription closed");
        if ev.session() != Some(&sid) {
            continue;
        }
        if let OutEvent::ToolRequest {
            request_id, input, ..
        } = &ev
        {
            assert!(
                input.contains("OUTSIDE the project root"),
                "escaping binding call must carry the ADR-0109 warning; got {input}"
            );
            saw_warning = true;
            holly
                .send(InMsg::Approve {
                    session: sid.clone(),
                    request_id: request_id.clone(),
                    scope: ApprovalScope::Session,
                })
                .await
                .unwrap();
        }
        let done = matches!(ev, OutEvent::Done { .. });
        if done {
            break;
        }
    }
    assert!(saw_warning, "expected an escape-root approval prompt");

    let target = outside.path.join("f.txt");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "hello world\n",
        "the approved binding call wrote outside the project root"
    );
    let canonical_target = outside.path.canonicalize().unwrap().join("f.txt");
    assert!(
        store.is_durably_allowed("write", &canonical_target),
        "the Session-scoped approval must be recorded into the ExtraRootStore"
    );
}

/// #446: a durable escape grant recorded earlier — e.g. by a direct, non-rhai
/// approved call — is honored by a rhai binding with no new prompt, exactly as
/// it already is for a direct tool call (defense-in-depth erosion, not a
/// bypass: the binding's own Allow/Ask/Deny grade still applies).
#[tokio::test]
async fn pre_existing_durable_grant_is_honored_without_a_new_prompt() {
    let dir = TempDir::new("escape-preexisting");
    let outside = TempDir::new("escape-preexisting-target");
    let outside_name = outside.path.file_name().unwrap().to_str().unwrap();
    let rel = format!("../{outside_name}/f.txt");
    let script = format!(r#"write("{rel}", "hello world\n")"#);

    let (holly, store) = spawn_with_rhai_escape(
        &script,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
    );
    let target = outside.path.canonicalize().unwrap().join("f.txt");
    store.record("write", &target, ApprovalScope::Session, "pre-existing");

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a pre-existing durable escape grant must not re-prompt"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path.join("f.txt")).unwrap(),
        "hello world\n",
        "the binding call ran through the pre-existing grant"
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

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ #419: call/bash exec bindings
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[tokio::test]
async fn call_binding_runs_argv_exec_when_allowed() {
    let dir = TempDir::new("call-allow");
    let holly = spawn_with_rhai_exec(
        r#"exec("echo", ["hi"])"#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
        false,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "Allow call must not ask for approval"
    );
    let out = rhai_output(&events).expect("expected rhai output");
    assert!(out.contains("hi"), "exec binding ran echo: {out}");
}

#[tokio::test]
async fn call_binding_denied_surfaces_as_catchable_script_error() {
    let dir = TempDir::new("call-deny");
    let holly = spawn_with_rhai_exec(
        r#"let r = ""; try { exec("echo", ["hi"]); r = "ran" } catch(e) { r = "caught: " + e } r"#,
        &dir.path,
        one_profile(
            "denycall",
            PermissionProfile::new(Permission::Allow).with("call", Permission::Deny),
        ),
        false,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "denycall").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("caught") && out.contains("denied"),
        "deny should throw a catchable error; got {out}"
    );
}

#[tokio::test]
async fn call_binding_masked_when_omitted_from_profile_tools() {
    let dir = TempDir::new("call-masked");
    let mut profiles = entanglement_runtime::agents::built_in_registry();
    profiles.insert(AgentProfile {
        name: "readonly".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Allow),
        // `rhai` itself must stay allowlisted so the run reaches the binding
        // mask being tested here — only `call` is omitted.
        tools: Some(vec!["read".into(), RHAI_TOOL.into()]),
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    });
    let holly = spawn_with_rhai_exec(
        r#"let r = ""; try { exec("echo", ["hi"]); r = "ran" } catch(e) { r = "caught: " + e } r"#,
        &dir.path,
        profiles,
        false,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "readonly").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("caught") && out.contains("restricted"),
        "call omitted from `tools` must mask the binding; got {out}"
    );
}

#[tokio::test]
async fn bash_binding_absent_without_bash_enabled() {
    let dir = TempDir::new("bash-absent");
    let holly = spawn_with_rhai_exec(
        r#"bash("echo hi")"#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
        false,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.to_lowercase().contains("function"),
        "bash() must be an unknown-function (unregistered) error when bash is \
         disabled, not a graded-then-failing binding: {out}"
    );
}

#[tokio::test]
async fn bash_binding_runs_when_enabled_and_allowed() {
    let dir = TempDir::new("bash-allow");
    let holly = spawn_with_rhai_exec(
        r#"bash("echo hi")"#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
        true,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert!(out.contains("hi"), "bash binding ran echo: {out}");
}

/// #419 fix A regression: `call`/`bash` approvals are cached per resolved
/// command line, not per bare tool name — approving `exec(echo a)` (asking
/// under the `call` tool) must not silently pre-clear the unrelated
/// `exec(echo b)` later in the same run.
#[tokio::test]
async fn approving_one_call_command_does_not_auto_clear_a_different_one() {
    let dir = TempDir::new("call-fix-a");
    // `print(...)` (not a returned value) so the raw call output — including
    // its real newlines — lands in the tool output verbatim; a *returned*
    // string instead gets JSON-serialized (escaped `\n`), which would make a
    // line-based assertion on the echoed output meaningless.
    let holly = spawn_with_rhai_exec(
        r#"print(exec("echo", ["a"])); print(exec("echo", ["b"]));"#,
        &dir.path,
        one_profile(
            "askcall",
            PermissionProfile::new(Permission::Allow).with("call", Permission::Ask),
        ),
        false,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "askcall").await;

    // `exec()` blocks the (synchronous) script until its approval resolves, so
    // the two calls' asks arrive one at a time, not concurrently — auto-approve
    // each as it comes and let the run finish, then check *how many* asks fired.
    let events = collect_auto_approving(&holly, sub, &sid).await;
    let requests = events
        .iter()
        .filter(|e| matches!(e, OutEvent::ToolRequest { .. }))
        .count();
    assert_eq!(
        requests, 2,
        "two distinct exec() commands must ask twice, not share one cache entry \
         (a cache keyed by bare tool name would collapse this to 1)"
    );
    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.lines().any(|l| l == "a") && out.lines().any(|l| l == "b"),
        "both calls ran: {out}"
    );
}

/// #480/ADR-0130: a workdir-scoped config-ceiling rule (`bash{pattern}`,
/// #425/ADR-0116) now fires for a rhai `bash(command, workdir)` binding call
/// — previously inert, since the binding never marshalled a `workdir` at all.
/// The same rule stays inert for a workdir-less binding call, matching a
/// direct tool call's behavior with no `workdir` argument.
#[tokio::test]
async fn bash_binding_workdir_scoped_rule_fires_for_matching_workdir() {
    let dir = TempDir::new("bash-workdir-rule");
    std::fs::create_dir_all(dir.path.join("sub")).unwrap();
    let base = PermissionProfile::new(Permission::Allow).with("bash{sub*}", Permission::Deny);
    let holly = spawn_with_rhai_exec_and_base(
        r#"
        let out = "";
        try { bash("echo hi", "sub"); out += "unexpectedly-ran;" }
        catch(e) { out += "denied:" + e + ";" }
        try { bash("echo hi"); out += "ran-ok;" }
        catch(e) { out += "unexpected-deny:" + e + ";" }
        out
        "#,
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
        true,
        base,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("expected rhai output");
    assert!(
        out.contains("denied:") && out.contains("denied by permission profile"),
        "bash{{sub*}} must deny bash(\"...\", \"sub\"): {out}"
    );
    assert!(
        out.contains("ran-ok;"),
        "a workdir-less bash(...) call must still run — the rule is inert \
         with no `workdir` marshalled: {out}"
    );
    assert!(
        !out.contains("unexpectedly-ran") && !out.contains("unexpected-deny"),
        "unexpected grade: {out}"
    );
}

/// Same-command re-invocation still hits the once-per-run cache (unchanged
/// behavior for `call`/`bash`, same as the quintet) — only a *different*
/// command line forces a fresh ask.
#[tokio::test]
async fn approving_a_call_command_covers_a_repeat_of_the_same_command() {
    let dir = TempDir::new("call-same-cmd");
    let holly = spawn_with_rhai_exec(
        r#"print(exec("echo", ["a"])); print(exec("echo", ["a"]));"#,
        &dir.path,
        one_profile(
            "askcall",
            PermissionProfile::new(Permission::Allow).with("call", Permission::Ask),
        ),
        false,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "askcall").await;
    let events = collect_auto_approving(&holly, sub, &sid).await;

    let requests = events
        .iter()
        .filter(|e| matches!(e, OutEvent::ToolRequest { .. }))
        .count();
    assert_eq!(
        requests, 1,
        "the same command line reuses the cached approval"
    );
    let out = rhai_output(&events).expect("expected rhai output");
    assert_eq!(
        out.lines().filter(|l| *l == "a").count(),
        2,
        "both calls ran: {out}"
    );
}

// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ┃ #477: the active skill's `allowed_tools` mask reaches rhai bindings
// ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Collect events for `sid` up to and including the *n*th `Done`, then linger
/// briefly to also catch anything the tool executor emits asynchronously right
/// after `Done` — mirrors `skill_mask.rs`'s helper of the same shape.
async fn collect_through_dones(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dones: usize,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let mut seen_dones = 0;
    while seen_dones < dones {
        let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
            break;
        };
        if ev.session() != Some(sid) {
            continue;
        }
        if matches!(ev, OutEvent::Done { .. }) {
            seen_dones += 1;
        }
        out.push(ev);
    }
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), sub.recv()).await {
        if ev.session() == Some(sid) {
            out.push(ev);
        }
    }
    out
}

/// #477: a skill loaded via `load_skill` scopes `rhai` bindings exactly like it
/// scopes generic tool dispatch (#400/ADR-0106) — a script running while a
/// restrictive skill is active cannot use its `edit` binding to reach a tool
/// the skill's `allowed_tools` excludes, and the same script succeeds once the
/// skill's scope clears at the turn's `Done`.
#[tokio::test]
async fn skill_mask_refuses_a_binding_then_clears_after_done() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-rhai-skillmask-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());

    // A skill whose `allowed_tools` covers `read`/`rhai` (so the script itself
    // can launch) but excludes `edit` — the binding under test.
    let skill_dir = root.join(".entanglement/skills/restricted");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: restricted\ndescription: a read-only skill\nallowed_tools: [read, rhai]\n---\n\
         Only read and rhai.\n",
    )
    .unwrap();
    std::fs::write(root.join("f.txt"), "before").unwrap();

    std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
    let skill_registry = Arc::new(load_registry(&root).unwrap());
    std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");
    let skills = Arc::new(RwLock::new(skill_registry));

    let mut tools = host_tools(root.clone());
    tools.register(ReadRawTool::new(root.clone()));
    tools.register(LoadSkillTool::new(skills.clone()));

    let script = r#"let r = ""; try { edit("f.txt", "before", "after"); r = "ran" } catch(e) { r = "caught: " + e } r"#;
    let rhai_call = |id: &str| ToolCall {
        id: id.into(),
        name: RHAI_TOOL.into(),
        input: serde_json::json!({ "script": script }).to_string(),
        provider_meta: None,
    };

    let scripted = Arc::new(vec![
        // Turn 1, round 1: activate the skill.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "l1".into(),
                name: "load_skill".into(),
                input: serde_json::json!({ "skill_name": "restricted" }).to_string(),
                provider_meta: None,
            }],
        },
        // Turn 1, round 2: the script's `edit` binding must be refused — the
        // skill's `allowed_tools` excludes it.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![rhai_call("r1")],
        },
        // Turn 1, round 3: finish — triggers `Done`, clearing the skill mask.
        LlmResponse {
            text: "turn1 done".into(),
            tool_calls: vec![],
        },
        // Turn 2, round 1: the identical script, unmasked — must succeed.
        LlmResponse {
            text: "".into(),
            tool_calls: vec![rhai_call("r2")],
        },
        LlmResponse {
            text: "turn2 done".into(),
            tool_calls: vec![],
        },
    ]);

    let profiles = entanglement_runtime::agents::built_in_registry();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        active.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        tools.shared(),
        Arc::new(RwLock::new(profiles)),
        skills,
        PermissionProfile::new(Permission::Allow),
        active,
        resolver,
        grants,
        Hooks::default(),
        None,
    );

    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "use the restricted skill"))
        .await
        .unwrap();
    let turn1 = collect_through_dones(&mut sub, &sid, 1).await;

    let out1 = rhai_output(&turn1).expect("expected turn 1 rhai output");
    assert!(
        out1.contains("caught")
            && out1.contains("not available while skill `restricted` is active"),
        "the edit binding must be refused by the active skill's allowed_tools; got {out1}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "before",
        "the masked edit binding must not touch the filesystem"
    );

    holly
        .send(InMsg::prompt(sid.clone(), "run it again"))
        .await
        .unwrap();
    let turn2 = collect_through_dones(&mut sub, &sid, 1).await;

    let out2 = rhai_output(&turn2).expect("expected turn 2 rhai output");
    assert!(
        out2.contains("ran") && !out2.contains("not available while skill"),
        "the binding must be unmasked once the skill's scope clears at Done; got {out2}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "after",
        "the unmasked edit binding must run in turn 2"
    );
}
