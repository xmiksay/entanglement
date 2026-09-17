//! Dispatch-time lazy MCP re-enable end-to-end (ADR-0201): a resumed
//! session's replay restores its tool-overlay/permission state but never
//! MCP registration (`ToolRegistry` is process-lifetime, never persisted),
//! so an unknown `mcp__<server>__*` call must self-heal — or, failing that,
//! answer truthfully — rather than fall back to the generic unknown-tool
//! hint. Drives the real permission-dispatch loop
//! (`tool_runner::spawn_tool_executor_with_policy`) with a real
//! `AvailableMcp` and a real local streamable-HTTP MCP server (mirrors
//! `mcp_http.rs`'s harness), never a stand-in `Tool` registered ahead of time
//! — the whole point is that the tool does *not* exist in the registry when
//! the model calls it.
//!
//! Needs `mcp-http` (the transport) and `serve` (axum, the fake server) —
//! the default feature set enables both.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use entanglement_core::{
    stream_from_response, Agent, AgentCatalog, Catalog, EngineConfig, Holly, InMsg, Llm,
    LlmRequest, LlmResponse, LlmStream, McpServerState, OutEvent, Permission, PermissionProfile,
    SessionId, ToolCall,
};
use entanglement_runtime::mcp::{AvailableMcp, McpServerConfig};
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_runner::{spawn_tool_executor_with_policy, DiscoverySurface};
use entanglement_runtime::{Tool, ToolRegistry};

fn test_http_client() -> entanglement_core::HttpClient {
    // Mirrors `mcp_http.rs`: never write real shared endpoint state from tests.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        std::env::set_var("ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE", "1");
    });
    entanglement_core::HttpClient::new().unwrap()
}

/// A minimal streamable-HTTP MCP server advertising one `ping` tool whose
/// call always answers `"pong"`.
async fn ping_mcp(Json(req): Json<Value>) -> Response {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();
    match method {
        "initialize" => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "protocolVersion": "2025-03-26", "serverInfo": { "name": "test" } }
        }))
        .into_response(),
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "tools": [
                { "name": "ping", "description": "pong it", "inputSchema": { "type": "object", "properties": {} } }
            ] }
        }))
        .into_response(),
        "tools/call" => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "content": [ { "type": "text", "text": "pong" } ] }
        }))
        .into_response(),
        _ => (StatusCode::BAD_REQUEST, "unknown method").into_response(),
    }
}

async fn spawn_ping_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route("/mcp", post(ping_mcp));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}/mcp")
}

/// An LLM that replays scripted responses in order, then plain text.
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

fn tool_call_response(id: &str, name: &str) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: "{}".into(),
            provider_meta: None,
        }],
    }
}

fn done_response() -> LlmResponse {
    LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    }
}

/// A single-mode table named `"build"` (matching `DEFAULT_MODE`) with the
/// given `default` grade and no rules — the mode-based analog of
/// `unmasked_profile`'s `perm` (ADR-0207 stage 4 grades from the session's
/// mode, not its `Agent`): `Ask` proves the ladder still runs after a
/// lazy enable; `Allow` keeps the other scenarios to one round-trip.
fn mode_table_with_default(default: Permission) -> Arc<entanglement_runtime::mode::ModeTable> {
    let mode = entanglement_runtime::mode::Mode {
        name: "build".to_string(),
        default,
        rules: entanglement_runtime::mode::Rules::default(),
        limits: entanglement_runtime::mode::Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(
        entanglement_runtime::mode::ModeTable::new(vec![mode]).expect("single-mode table is valid"),
    )
}

/// `perm` is unused here (the profile carries no permission fact any more,
/// ADR-0207) but kept as a parameter since every call site also feeds it to
/// [`mode_table_with_default`] to build the mode that actually grades.
fn unmasked_profile(name: &str, _perm: Permission) -> AgentCatalog {
    let mut profiles = AgentCatalog::default();
    profiles.insert(Agent {
        name: name.into(),
        description: String::new(),
        system_prompt: String::new(),
        model: None,
        provider: None,
    });
    profiles
}

/// Also registers a plain `bash` tool so `unknown_tool_message`'s
/// Levenshtein hint has something to (not) match against.
struct EchoBash;
#[async_trait]
impl Tool for EchoBash {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

/// Wire a full `spawn_tool_executor_with_policy` around `avail`/`active`, one
/// scripted LLM turn, and `profiles`. `active` is a fresh, empty
/// `ActiveServers` every time — the whole point of every test here is that
/// the called tool is *not* already registered/connected.
fn spawn_executor(
    agents: AgentCatalog,
    scripted: Vec<LlmResponse>,
    avail: AvailableMcp,
    mode_table: Arc<entanglement_runtime::mode::ModeTable>,
) -> Holly {
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm::new(scripted.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let shared_tools = reg.shared();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(agents)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        active,
        perm_modes,
        resolver,
        grants,
        Default::default(),
        None,
        mode_table,
        Arc::new(PlanFileRegistry::new()),
        None,
        None,
        Some(DiscoverySurface {
            mcp_avail: Arc::new(avail),
            mcp_active: Arc::new(Mutex::new(HashMap::new())),
            http: Some(test_http_client()),
            ..Default::default()
        }),
    );
    holly
}

async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    // A connect-failure path retries through the shared endpoint pool's
    // default backoff (5 attempts, ~200ms doubling — several real seconds)
    // before it gives up, so this stays generous rather than tuned tight.
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(20), sub.recv()).await {
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

fn user_mcp_entry(url: &str, state: McpServerState) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        disabled: false,
        capabilities: HashMap::new(),
        oauth: None,
        state: Some(state),
    }
}

/// The primary fix: an `allowed`-tier server's tool, never registered this
/// process, is transparently lazily connected on the model's first call —
/// and the rest of the permission ladder still runs on it (`Ask` still
/// prompts, proving the self-heal doesn't bypass grading).
#[tokio::test]
async fn allowed_tier_self_heals_and_the_ladder_still_runs() {
    let url = spawn_ping_server().await;
    let mut user = HashMap::new();
    user.insert(
        "testsrv".to_string(),
        user_mcp_entry(&url, McpServerState::Allowed),
    );
    let (_startup, avail) = AvailableMcp::partition(&Catalog { providers: vec![] }, &user, vec![]);

    let profiles = unmasked_profile("mcptest", Permission::Ask);
    let scripted = vec![
        tool_call_response("t1", "mcp__testsrv__ping"),
        done_response(),
    ];
    let holly = spawn_executor(
        profiles,
        scripted,
        avail,
        mode_table_with_default(Permission::Ask),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "mcptest".into(),
            prompt: String::new(),
            user: None,
        })
        .await
        .unwrap();
    let mut watch = holly.subscribe();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    // The ladder must still ask — a lazy re-enable restores registration,
    // it does not grant permission.
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "mcp__testsrv__ping") {
            asked = true;
            break;
        }
    }
    assert!(asked, "the lazy-enabled tool must still go through Ask");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: entanglement_core::ApprovalScope::Once,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    let output = events.iter().find_map(|e| match e {
        OutEvent::ToolOutput { output, .. } => Some(output.clone()),
        _ => None,
    });
    assert_eq!(
        output.as_deref(),
        Some("pong"),
        "the real server must have been called after the self-heal; got {events:?}"
    );
}

/// A `disabled`-tier server is a **known** name with consent explicitly
/// withheld — never the generic unknown-tool message.
#[tokio::test]
async fn disabled_tier_gets_a_truthful_decline_not_unknown_tool() {
    let mut user = HashMap::new();
    user.insert(
        "offsrv".to_string(),
        user_mcp_entry("http://127.0.0.1:1/mcp", McpServerState::Disabled),
    );
    let (_startup, avail) = AvailableMcp::partition(&Catalog { providers: vec![] }, &user, vec![]);

    let profiles = unmasked_profile("mcptest", Permission::Allow);
    let scripted = vec![
        tool_call_response("t1", "mcp__offsrv__anything"),
        done_response(),
    ];
    let holly = spawn_executor(
        profiles,
        scripted,
        avail,
        mode_table_with_default(Permission::Allow),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "mcptest".into(),
            prompt: String::new(),
            user: None,
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    let (output, is_error) = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("a ToolOutput must have been emitted");
    assert!(is_error, "a disabled-tier call must set is_error; {output}");
    assert!(
        output.contains("disabled by configuration"),
        "got: {output}"
    );
    assert!(
        !output.contains("unknown tool"),
        "a known-but-disabled server must never be reported unknown; got: {output}"
    );
}

/// A name matching no registered tool and no configured/bundled server in
/// any tier keeps the ordinary unknown-tool hint, unchanged.
#[tokio::test]
async fn genuinely_unknown_tool_keeps_the_unknown_tool_hint() {
    let (_startup, avail) =
        AvailableMcp::partition(&Catalog { providers: vec![] }, &HashMap::new(), vec![]);
    let profiles = unmasked_profile("mcptest", Permission::Allow);
    let scripted = vec![
        tool_call_response("t1", "totally_bogus_tool_zzz"),
        done_response(),
    ];
    let holly = spawn_executor(
        profiles,
        scripted,
        avail,
        mode_table_with_default(Permission::Allow),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "mcptest".into(),
            prompt: String::new(),
            user: None,
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    let output = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a ToolOutput must have been emitted");
    assert!(output.contains("unknown tool"), "got: {output}");
}

/// A connect failure is a distinguishable error, not "unknown tool" — and a
/// second call against the same broken server within the cooldown window
/// short-circuits to a different, guard-specific message instead of
/// re-attempting the connect (ADR-0201's stampede guard).
#[tokio::test]
async fn enable_failure_is_distinguishable_and_a_repeat_call_is_guarded() {
    let mut user = HashMap::new();
    // No listener on this port: the connect fails fast (refused), unlike a
    // black-holed address which would eat the full connect timeout.
    user.insert(
        "brokensrv".to_string(),
        user_mcp_entry("http://127.0.0.1:1/mcp", McpServerState::Allowed),
    );
    let (_startup, avail) = AvailableMcp::partition(&Catalog { providers: vec![] }, &user, vec![]);

    let profiles = unmasked_profile("mcptest", Permission::Allow);
    let scripted = vec![
        tool_call_response("t1", "mcp__brokensrv__x"),
        done_response(),
        tool_call_response("t2", "mcp__brokensrv__y"),
        done_response(),
    ];
    let holly = spawn_executor(
        profiles,
        scripted,
        avail,
        mode_table_with_default(Permission::Allow),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "mcptest".into(),
            prompt: String::new(),
            user: None,
        })
        .await
        .unwrap();

    let sub1 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events1 = collect(sub1, &sid).await;
    let output1 = events1
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("a ToolOutput must have been emitted");
    assert!(
        output1.1,
        "a connect failure must set is_error; {output1:?}"
    );
    assert!(
        output1.0.contains("could not be re-enabled"),
        "got: {}",
        output1.0
    );
    assert!(!output1.0.contains("unknown tool"), "got: {}", output1.0);

    let sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "go again"))
        .await
        .unwrap();
    let events2 = collect(sub2, &sid).await;
    let output2 = events2
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a second ToolOutput must have been emitted");
    assert!(
        output2.contains("recent connect failure"),
        "the second call within the cooldown must short-circuit rather than \
         reattempt the connect; got: {output2}"
    );
}
