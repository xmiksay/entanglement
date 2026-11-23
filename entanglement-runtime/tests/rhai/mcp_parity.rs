//! #843: per-tool MCP control is a `mcp__<server>__<tool>` rule or overlay
//! entry (a server-wide `mcp__<server>__*` glob in the overlay only — rule
//! keys match the tool part exactly), and it holds on both ways a model reaches an MCP tool — the `mcp`
//! envelope (core unwraps it to the inner name) and a script's generated
//! `mcp::<server>::<tool>()` binding (ADR-0227). A per-tool mode `deny` and a
//! session overlay deny each refuse both calls with the same text, and the
//! stub server never sees a `tools/call`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use entanglement_core::{
    Catalog, EngineConfig, Holly, InMsg, Llm, LlmResponse, OutEvent, Permission, PermissionProfile,
    SessionId, ToolCall, ToolOverlayEntry,
};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::mcp::{AvailableMcp, McpServerConfig, McpToolCache};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::AdvertisingState;
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
use serde_json::{json, Value};

use super::{collect_auto_approving, ScriptedLlm, TempDir};

/// The stub server's one tool, under the name every gate grades.
const REGISTERED: &str = "mcp__stub-srv__call";

/// A streamable-HTTP MCP server with one `call` tool echoing its `msg`,
/// counting every `tools/call` it answers.
async fn stub(State(calls): State<Arc<AtomicUsize>>, Json(req): Json<Value>) -> Response {
    let id = req.get("id").cloned();
    match req.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => Json(json!({"jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": "2025-03-26", "serverInfo": {"name": "stub"}}}))
        .into_response(),
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => Json(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
            {"name": "call", "description": "echo", "inputSchema": {"type": "object",
             "properties": {"msg": {"type": "string"}}, "required": ["msg"]}}]}}))
        .into_response(),
        "tools/call" => {
            calls.fetch_add(1, Ordering::SeqCst);
            let msg = req["params"]["arguments"]["msg"].as_str().unwrap_or("?");
            Json(json!({"jsonrpc": "2.0", "id": id, "result": {
                "content": [{"type": "text", "text": format!("echo:{msg}")}]}}))
            .into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "unknown").into_response(),
    }
}

/// How the call reaches the MCP tool.
#[derive(Clone, Copy, Debug)]
enum Path {
    Envelope,
    Script,
}

/// The policy under test.
#[derive(Clone, Copy, Debug)]
enum Policy {
    None,
    ModeDeny(&'static str),
    OverlayDeny(&'static str),
}

/// What one run produced: the call's tool output, whether any approval card
/// (other than the `rhai` launch's) was raised, and the server's
/// `tools/call` count.
struct Run {
    output: String,
    carded: bool,
    server_calls: usize,
}

/// One session `s1` on a production-shaped engine (the real
/// `tool_spec_resolver`, so core unwraps the envelope) whose model makes one
/// call to the stub server's tool along `path`, under `policy`.
async fn run(path: Path, policy: Policy) -> Run {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/mcp", post(stub))
        .with_state(calls.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let tag = match policy {
        Policy::None => "none",
        Policy::ModeDeny(_) => "mode",
        Policy::OverlayDeny(_) => "overlay",
    };
    let dir = TempDir::new(&format!("mcp-parity-{path:?}-{tag}"));
    // The last-seen list generates the script's `mcp::stub_srv::call_`; the
    // envelope's inner name connects lazily on its own.
    let cache = McpToolCache::at(Some(dir.path.join("mcp-tools.yml")));
    cache
        .record("stub-srv", &url, &["call".to_string()])
        .unwrap();
    let cfg = McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url),
        headers: HashMap::new(),
        disabled: false,
        capabilities: HashMap::new(),
        oauth: None,
        description: None,
    };
    let avail = AvailableMcp::from_config(
        &Catalog { providers: vec![] },
        &HashMap::from([("stub-srv".to_string(), cfg)]),
        vec![],
    )
    .with_tool_cache(cache);

    let call = match path {
        Path::Envelope => ToolCall {
            id: "t1".into(),
            name: "mcp".into(),
            input: json!({"server": "stub-srv", "tool": "call", "args": {"msg": "hi"}}).to_string(),
            provider_meta: None,
        },
        Path::Script => ToolCall {
            id: "t1".into(),
            name: RHAI_TOOL.into(),
            input: json!({"script": r#"let r = "";
                try { r = mcp::stub_srv::call_(#{msg: "hi"}); } catch (e) { r = "" + e; }
                r"#})
            .to_string(),
            provider_meta: None,
        },
    };
    let scripted = vec![
        LlmResponse {
            tool_calls: vec![call],
            ..Default::default()
        },
        LlmResponse {
            text: "ok".into(),
            ..Default::default()
        },
    ];
    let deny: Vec<String> = match policy {
        Policy::ModeDeny(rule) => vec![rule.to_string()],
        _ => vec![],
    };
    let mode = Mode {
        name: "build".into(),
        default: Permission::Allow,
        rules: Rules::from_lists(&deny, &[], &[]),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let tools = host_tools(dir.path.clone()).shared();
    let advertising = Arc::new(AdvertisingState::new());
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm::new(scripted.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: tools.clone(),
            advertising: advertising.clone(),
            agent_specs: entanglement_runtime::subagent::agent_specs(),
        })),
        ..EngineConfig::default()
    });
    let _executor = ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .mode_table(Arc::new(ModeTable::new(vec![mode]).unwrap()))
        .discovery(DiscoverySurface {
            advertising,
            mcp_avail: Arc::new(avail),
            mcp_active: Arc::new(Mutex::new(HashMap::new())),
            http: Some(
                entanglement_core::HttpClient::with_shared_state(
                    entanglement_core::SharedStateLocation::Disabled,
                    entanglement_core::RetryConfig::default(),
                )
                .unwrap(),
            ),
            ..Default::default()
        })
        .spawn();

    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    if let Policy::OverlayDeny(pattern) = policy {
        holly
            .send(InMsg::SetToolOverlay {
                session: sid.clone(),
                entries: vec![ToolOverlayEntry::deny(pattern)],
            })
            .await
            .unwrap();
    }
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(20),
        collect_auto_approving(&holly, sub, &sid),
    )
    .await
    .unwrap();
    if let Policy::OverlayDeny(_) = policy {
        assert!(
            events
                .iter()
                .any(|e| matches!(e, OutEvent::ToolOverlayChanged { .. })),
            "the overlay was applied: {events:?}"
        );
    }
    let output = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                request_id, output, ..
            } if request_id == "t1" => Some(output.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no ToolOutput for t1: {events:?}"));
    Run {
        output,
        carded: events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        server_calls: calls.load(Ordering::SeqCst),
    }
}

/// The control: with no policy both paths reach the server, so the denies
/// below are not vacuous.
#[tokio::test]
async fn with_no_policy_both_paths_reach_the_server() {
    for path in [Path::Envelope, Path::Script] {
        let r = run(path, Policy::None).await;
        assert!(r.output.contains("echo:hi"), "{path:?}: {}", r.output);
        assert_eq!(r.server_calls, 1, "{path:?}");
    }
}

/// Both paths are refused under `policy`, with the same refusal text, no
/// card, and no `tools/call` — returns that text.
async fn refused_identically(policy: Policy) -> String {
    let envelope = run(Path::Envelope, policy).await;
    let script = run(Path::Script, policy).await;
    for (path, r) in [("envelope", &envelope), ("script", &script)] {
        assert!(!r.carded, "{path} under {policy:?} raised a card");
        assert_eq!(
            r.server_calls, 0,
            "{path} under {policy:?} reached the server"
        );
        assert!(!r.output.contains("echo:"), "{path}: {}", r.output);
    }
    assert!(
        envelope.output.contains(REGISTERED),
        "the refusal names the real tool: {}",
        envelope.output
    );
    assert!(
        script.output.contains(envelope.output.trim()),
        "same refusal on both paths:\n envelope: {}\n script: {}",
        envelope.output,
        script.output
    );
    envelope.output
}

#[tokio::test]
async fn a_per_tool_mode_deny_refuses_the_envelope_and_the_script_alike() {
    let why = refused_identically(Policy::ModeDeny(REGISTERED)).await;
    assert!(why.contains("denied by mode `build`"), "{why}");
}

#[tokio::test]
async fn an_overlay_deny_refuses_the_envelope_and_the_script_alike() {
    let why = refused_identically(Policy::OverlayDeny("mcp__stub-srv__*")).await;
    assert!(
        why.contains("withdrawn for this session by /disable"),
        "{why}"
    );
}
