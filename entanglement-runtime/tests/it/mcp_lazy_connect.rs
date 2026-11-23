//! Lazy MCP connect end-to-end (ADR-0226, keeping ADR-0201's dispatch-time
//! connect): nothing connects at startup; `list_mcp` lists a registered
//! server without connecting it (tool names from the last-seen cache);
//! `mcp_tools` connects it and reads its catalog; a direct `mcp(...)` on a
//! server never read connects it at dispatch and is still graded; a
//! `disabled` server is declined truthfully; a failed connect is a
//! distinguishable, cooldown-guarded error. Drives a real engine + executor
//! against a real local streamable-HTTP MCP server — the tool does *not*
//! exist in the registry when the model calls it.
//!
//! Needs `mcp-http` (the transport) and `serve` (axum, the fake server) —
//! the default feature set enables both.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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
    LlmRequest, LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, SessionId,
    ToolCall,
};
use entanglement_runtime::mcp::{AvailableMcp, McpServerConfig, McpToolCache};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::AdvertisingState;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
use entanglement_runtime::{Tool, ToolRegistry};

fn test_http_client() -> entanglement_core::HttpClient {
    // Mirrors `mcp_http.rs`: never write real shared endpoint state from tests.
    entanglement_core::HttpClient::with_shared_state(
        entanglement_core::SharedStateLocation::Disabled,
        entanglement_core::RetryConfig::default(),
    )
    .unwrap()
}

/// A minimal streamable-HTTP MCP server advertising one `ping` tool (one
/// required `msg` string) whose call always answers `"pong"`.
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
                { "name": "ping", "description": "pong it", "inputSchema": {
                    "type": "object",
                    "properties": { "msg": { "type": "string" } },
                    "required": ["msg"]
                } }
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
                ..Default::default()
            });
        Ok(stream_from_response(resp))
    }
}

fn tool_call_response(id: &str, name: &str, input: &str) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

fn done_response() -> LlmResponse {
    LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
        ..Default::default()
    }
}

/// A single-mode table named `"build"` (matching `DEFAULT_MODE`) with the
/// given `default` grade and no rules: `Ask` proves the ladder still runs
/// after a lazy connect; `Allow` keeps the other scenarios to one round-trip.
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

fn profiles() -> AgentCatalog {
    let mut profiles = AgentCatalog::default();
    profiles.insert(Agent {
        name: "mcptest".into(),
        description: String::new(),
        system_prompt: String::new(),
        model: None,
        provider: None,
        default_mode: None,
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

/// Wire a full engine (the production `tool_spec_resolver`, so core unwraps
/// `mcp` calls) + `ToolExecutor::builder` around `avail` with an empty
/// `ActiveServers` — the whole point is that nothing is connected when the
/// model starts — then run one prompt on a fresh session and return its
/// events.
async fn run_turns(
    scripted: Vec<LlmResponse>,
    avail: AvailableMcp,
    mode_table: Arc<entanglement_runtime::mode::ModeTable>,
    prompts: &[&str],
) -> Vec<Vec<OutEvent>> {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let shared_tools = reg.shared();
    let advertising = Arc::new(AdvertisingState::new());
    let agents = profiles();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm::new(scripted.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: shared_tools.clone(),
            advertising: advertising.clone(),
            agent_specs: Vec::new(),
        })),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let perm_modes = crate::mode_support::perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load_from(None));
    let _executor = ToolExecutor::builder(&holly, shared_tools)
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(mode_table)
        .discovery(DiscoverySurface {
            advertising,
            mcp_avail: Arc::new(avail),
            mcp_active: Arc::new(Mutex::new(HashMap::new())),
            http: Some(test_http_client()),
            ..Default::default()
        })
        .spawn();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "mcptest".into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    let mut turns = Vec::new();
    for prompt in prompts {
        let sub = holly.subscribe();
        holly
            .send(InMsg::prompt(sid.clone(), *prompt))
            .await
            .unwrap();
        turns.push(collect(sub, &sid).await);
    }
    turns
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

fn mcp_entry(url: &str, disabled: bool) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        disabled,
        capabilities: HashMap::new(),
        oauth: None,
        description: Some("a test ping server".into()),
    }
}

fn roster(name: &str, cfg: McpServerConfig) -> AvailableMcp {
    let user = HashMap::from([(name.to_string(), cfg)]);
    AvailableMcp::from_config(&Catalog { providers: vec![] }, &user, vec![])
}

/// Every `(request_id, output, is_error)` in `events`, in order.
fn outputs(events: &[OutEvent]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput {
                request_id,
                output,
                is_error,
                ..
            } => Some((request_id.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// `list_mcp` lists without connecting, `mcp_tools` connects and delivers the
/// schema + call line, a second `list_mcp` shows it connected with its tool,
/// and the connect recorded the tool names in the last-seen cache.
#[tokio::test]
async fn list_never_connects_mcp_tools_connects_and_the_cache_records_it() {
    let url = spawn_ping_server().await;
    let dir = tempfile::tempdir().unwrap();
    let cache = McpToolCache::at(Some(dir.path().join("mcp-tools.yml")));
    let avail = roster("testsrv", mcp_entry(&url, false)).with_tool_cache(cache.clone());
    let scripted = vec![
        tool_call_response("l1", "list_mcp", "{}"),
        tool_call_response("t1", "mcp_tools", r#"{"name":"testsrv"}"#),
        tool_call_response("l2", "list_mcp", "{}"),
        done_response(),
    ];
    let turns = run_turns(
        scripted,
        avail,
        mode_table_with_default(Permission::Allow),
        &["go"],
    )
    .await;
    let out = outputs(&turns[0]);
    assert_eq!(out.len(), 3, "{out:#?}");
    let (_, first_list, _) = &out[0];
    assert!(
        first_list.contains("testsrv — a test ping server [available]"),
        "{first_list}"
    );
    let (_, tools, is_error) = &out[1];
    assert!(!is_error, "{tools}");
    assert!(tools.contains("- ping: pong it"), "{tools}");
    assert!(tools.contains(r#""required":["msg"]"#), "{tools}");
    assert!(
        tools.contains(
            r#"call as: mcp {"server": "testsrv", "tool": "ping", "args": {"msg":"example"}}"#
        ),
        "{tools}"
    );
    let (_, second_list, _) = &out[2];
    assert!(
        second_list.contains("[connected]\n    tools: ping"),
        "{second_list}"
    );
    assert_eq!(
        cache.lookup("testsrv", &url),
        Some(vec!["ping".to_string()])
    );
}

/// Before any connect, `list_mcp` shows the last-seen tool names.
#[tokio::test]
async fn list_mcp_shows_last_seen_tool_names_for_an_unconnected_server() {
    let dir = tempfile::tempdir().unwrap();
    let cache = McpToolCache::at(Some(dir.path().join("mcp-tools.yml")));
    cache
        .record(
            "testsrv",
            "http://127.0.0.1:1/mcp",
            &["ping".into(), "echo".into()],
        )
        .unwrap();
    let avail =
        roster("testsrv", mcp_entry("http://127.0.0.1:1/mcp", false)).with_tool_cache(cache);
    let turns = run_turns(
        vec![tool_call_response("l1", "list_mcp", "{}"), done_response()],
        avail,
        mode_table_with_default(Permission::Allow),
        &["go"],
    )
    .await;
    let out = outputs(&turns[0]);
    assert!(
        out[0]
            .1
            .contains("[available]\n    tools (last seen): ping, echo"),
        "{out:#?}"
    );
}

/// A direct `mcp(...)` on a server never read with `mcp_tools` connects at
/// dispatch — and the ladder still runs (`Ask` still prompts): a lazy
/// connect restores registration, it grants nothing.
#[tokio::test]
async fn a_direct_mcp_call_connects_at_dispatch_and_is_still_graded() {
    let url = spawn_ping_server().await;
    let avail = roster("testsrv", mcp_entry(&url, false));
    let scripted = vec![
        tool_call_response(
            "t1",
            "mcp",
            r#"{"server":"testsrv","tool":"ping","args":{"msg":"hi"}}"#,
        ),
        done_response(),
    ];
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let shared_tools = reg.shared();
    let advertising = Arc::new(AdvertisingState::new());
    let agents = profiles();
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm::new(scripted.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: shared_tools.clone(),
            advertising: advertising.clone(),
            agent_specs: Vec::new(),
        })),
        ..EngineConfig::default()
    });
    let mode_table = mode_table_with_default(Permission::Ask);
    let perm_modes = crate::mode_support::perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let _executor = ToolExecutor::builder(&holly, shared_tools)
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(Arc::new(DefaultGrantStore::load_from(None)) as Arc<dyn GrantStore>)
        .mode_table(mode_table)
        .discovery(DiscoverySurface {
            advertising,
            mcp_avail: Arc::new(avail),
            mcp_active: Arc::new(Mutex::new(HashMap::new())),
            http: Some(test_http_client()),
            ..Default::default()
        })
        .spawn();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "mcptest".into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    let mut watch = holly.subscribe();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "mcp__testsrv__ping") {
            asked = true;
            break;
        }
    }
    assert!(asked, "the lazily connected tool must still go through Ask");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: entanglement_core::ApprovalScope::Once,
            mode: None,
            run_all: false,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    let out = outputs(&events);
    assert_eq!(
        out.first().map(|o| o.1.as_str()),
        Some("pong"),
        "{events:?}"
    );
}

/// A `disabled` server is a known name, deliberately off — never "unknown",
/// never connected, for both `mcp_tools` and a direct call.
#[tokio::test]
async fn a_disabled_server_gets_a_truthful_decline() {
    let avail = roster("offsrv", mcp_entry("http://127.0.0.1:1/mcp", true));
    let scripted = vec![
        tool_call_response("t1", "mcp_tools", r#"{"name":"offsrv"}"#),
        tool_call_response("t2", "mcp", r#"{"server":"offsrv","tool":"anything"}"#),
        done_response(),
    ];
    let turns = run_turns(
        scripted,
        avail,
        mode_table_with_default(Permission::Allow),
        &["go"],
    )
    .await;
    for (id, output, is_error) in outputs(&turns[0]) {
        assert!(is_error, "{id}: {output}");
        assert!(
            output.contains("disabled by configuration"),
            "{id}: {output}"
        );
    }
}

/// A connect failure is a distinguishable error — and a second attempt
/// within the cooldown window short-circuits instead of reconnecting
/// (ADR-0201's stampede guard).
#[tokio::test]
async fn a_failed_connect_is_distinguishable_and_a_repeat_is_guarded() {
    // No listener on this port: the connect fails fast (refused).
    let avail = roster("brokensrv", mcp_entry("http://127.0.0.1:1/mcp", false));
    let scripted = vec![
        tool_call_response("t1", "mcp_tools", r#"{"name":"brokensrv"}"#),
        done_response(),
        tool_call_response("t2", "mcp", r#"{"server":"brokensrv","tool":"y"}"#),
        done_response(),
    ];
    let turns = run_turns(
        scripted,
        avail,
        mode_table_with_default(Permission::Allow),
        &["go", "go again"],
    )
    .await;
    let first = outputs(&turns[0]);
    assert!(first[0].2, "{first:?}");
    assert!(first[0].1.contains("could not be connected"), "{first:?}");
    assert!(!first[0].1.contains("unknown tool"), "{first:?}");
    let second = outputs(&turns[1]);
    assert!(
        second[0].1.contains("recent connect failure"),
        "the second attempt within the cooldown must short-circuit: {second:?}"
    );
}
