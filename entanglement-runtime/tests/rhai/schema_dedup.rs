//! #844: a script's schema-violating call is invisible to the model's
//! dedup and loop-breaker state — it never marks a schema delivered, never
//! feeds the two-identical-failures streak, and its own decline carries the
//! schema with a rhai-path example instead of the `mcp` envelope.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use entanglement_core::{
    Catalog, EngineConfig, Holly, InMsg, Llm, LlmResponse, OutEvent, Permission, PermissionProfile,
    SessionId, ToolCall,
};
use entanglement_runtime::arg_validate::LOOP_BREAKER_NOTE;
use entanglement_runtime::host::host_tools;
use entanglement_runtime::mcp::{AvailableMcp, McpServerConfig, McpToolCache};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
use serde_json::{json, Value};

use super::{collect_auto_approving, ScriptedLlm, TempDir};

/// The bad call both the script and the model make: `msg` is required.
const BAD: &str = r#"{"bad":1}"#;

/// A streamable-HTTP MCP server with one `call` tool requiring `msg`.
async fn stub(Json(req): Json<Value>) -> Response {
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
        "tools/call" => Json(json!({"jsonrpc": "2.0", "id": id, "result": {
            "content": [{"type": "text", "text": "reached the server"}]}}))
        .into_response(),
        _ => (StatusCode::BAD_REQUEST, "unknown").into_response(),
    }
}

fn call(id: &str, name: &str, input: &str) -> LlmResponse {
    LlmResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

/// One turn: a `rhai` script makes the bad call twice (each caught), then
/// the model makes it directly twice (`m1`, `m2`).
async fn run_turn() -> Vec<OutEvent> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, Router::new().route("/mcp", post(stub))).await;
    });
    let dir = TempDir::new("schema-dedup");
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
    let script = r#"let out = [];
        for i in 0..2 { try { mcp::stub_srv::call_(#{bad: 1}); } catch (e) { out.push(e); } }
        out"#;
    let rhai_input = json!({ "script": script, "timeout": 20 }).to_string();
    let scripted = vec![
        call("t1", RHAI_TOOL, &rhai_input),
        call("m1", "mcp__stub-srv__call", BAD),
        call("m2", "mcp__stub-srv__call", BAD),
        LlmResponse {
            text: "ok".into(),
            ..Default::default()
        },
    ];
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm::new(scripted.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    let mode = Mode {
        name: "build".into(),
        default: Permission::Allow,
        rules: Rules::from_lists(&[], &[], &[]),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    let _executor = ToolExecutor::builder(&holly, host_tools(dir.path.clone()).shared())
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .mode_table(Arc::new(ModeTable::new(vec![mode]).unwrap()))
        .discovery(DiscoverySurface {
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
    let sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            prompt: "go".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        collect_auto_approving(&holly, sub, &sid),
    )
    .await
    .unwrap()
}

fn output_for(events: &[OutEvent], id: &str) -> String {
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                request_id, output, ..
            } if request_id == id => Some(output.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no ToolOutput for {id}: {events:?}"))
}

#[tokio::test]
async fn a_script_violation_neither_marks_the_schema_delivered_nor_feeds_the_loop_breaker() {
    let events = run_turn().await;

    // The script's own declines: full schema, rhai-path example, and no
    // loop-breaker note although its two calls were identical — the one
    // place a script-fed streak would show (the `rhai` call's own result is
    // a model call and resets the streak before `m1`).
    let script = output_for(&events, "t1");
    assert!(script.contains("missing required: msg"), "{script}");
    assert!(script.contains("correct usage"), "{script}");
    assert!(!script.contains("already provided"), "{script}");
    assert!(
        script.contains(r#"mcp::stub_srv::call_(#{msg: \"example\"})"#)
            || script.contains(r#"mcp::stub_srv::call_(#{msg: "example"})"#),
        "{script}"
    );
    assert!(!script.contains("through mcp"), "{script}");
    assert!(!script.contains(LOOP_BREAKER_NOTE), "{script}");
    assert!(!script.contains("reached the server"), "{script}");

    // The model's first direct bad call: the script's declines never marked
    // the schema delivered, so the model gets it in full.
    let m1 = output_for(&events, "m1");
    assert!(m1.contains("correct usage"), "{m1}");
    assert!(!m1.contains("already provided"), "{m1}");

    // Positive controls: the model's own repeat dedups and trips the breaker.
    let m2 = output_for(&events, "m2");
    assert!(m2.contains("already provided above"), "{m2}");
    assert!(m2.contains(LOOP_BREAKER_NOTE), "{m2}");
}
