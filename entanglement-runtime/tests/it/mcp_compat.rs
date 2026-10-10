//! MCP client compatibility over the streamable-HTTP transport (#727):
//! `tools/list` pagination (and its loop cap), `isError` reaching the
//! structured `is_error` side channel (ADR-0176), binary content collapsing
//! to a placeholder, and a server `ping` sent mid-call on the SSE stream
//! being answered. Drives the real transport against a fake axum server and
//! dispatches through a real [`ToolRegistry`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use entanglement_core::{ContentPart, SessionId, ToolCall};
use entanglement_runtime::mcp::tool::McpTool;
use entanglement_runtime::mcp::{HttpClient, McpClient};
use entanglement_runtime::tools::{ToolExecution, ToolRegistry};

/// Never touches the real data dir: shared endpoint state is disabled (#743).
/// Unpaced, so the default 50-RPM spacing doesn't add seconds per request;
/// `concurrency` is the endpoint's in-flight cap.
fn test_http_client(concurrency: usize) -> entanglement_core::HttpClient {
    entanglement_core::HttpClient::with_shared_state(
        entanglement_core::SharedStateLocation::Disabled,
        entanglement_core::RetryConfig {
            rpm: 1_000_000,
            concurrency,
            ..entanglement_core::RetryConfig::default()
        },
    )
    .unwrap()
}

/// A 1×1 PNG, base64 — the payload the fake `pic` tool returns.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[derive(Clone, Default)]
struct Fake {
    /// `tools/list` never stops handing out a `nextCursor`.
    looping: bool,
    /// Every JSON-RPC *response* the client POSTed back (its `ping` replies).
    replies: Arc<Mutex<Vec<Value>>>,
}

fn rpc(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn sse(events: &[Value]) -> Response {
    let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn mcp(State(fake): State<Fake>, Json(req): Json<Value>) -> Response {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        fake.replies.lock().unwrap().push(req);
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method {
        "initialize" => json!({ "protocolVersion": "2025-03-26", "serverInfo": { "name": "t" } }),
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" if fake.looping => json!({ "tools": [], "nextCursor": "again" }),
        "tools/list" => match req["params"].get("cursor").and_then(Value::as_str) {
            None => {
                json!({ "tools": [ { "name": "fail" }, { "name": "pic" } ], "nextCursor": "p2" })
            }
            Some("p2") => json!({ "tools": [ { "name": "pinger" } ] }),
            Some(other) => panic!("unexpected cursor {other}"),
        },
        "tools/call" => match req["params"]["name"].as_str().unwrap_or("") {
            "fail" => {
                json!({ "isError": true, "content": [ { "type": "text", "text": "quota exceeded" } ] })
            }
            "pic" => json!({ "content": [
                { "type": "text", "text": "chart:" },
                { "type": "image", "data": PNG, "mimeType": "image/png" },
                { "type": "resource_link", "uri": "https://ex.com/r", "name": "r" },
            ] }),
            // The ping reuses our request's id on purpose: it must be
            // answered, not mistaken for the response that follows it.
            "pinger" => {
                let ping = json!({ "jsonrpc": "2.0", "id": id, "method": "ping" });
                let done = rpc(
                    &id,
                    json!({ "content": [ { "type": "text", "text": "pong" } ] }),
                );
                return sse(&[ping, done]);
            }
            other => panic!("unexpected tool {other}"),
        },
        other => panic!("unexpected method {other}"),
    };
    Json(rpc(&id, result)).into_response()
}

async fn connect(fake: Fake, concurrency: usize) -> HttpClient {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route("/mcp", post(mcp)).with_state(fake);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = format!("http://127.0.0.1:{port}/mcp");
    HttpClient::connect(
        "srv",
        &url,
        &HashMap::new(),
        test_http_client(concurrency),
        None,
        None,
    )
    .await
    .expect("handshake")
}

/// Discover every tool (all pages) and register them as `mcp__srv__*`.
async fn registry(fake: Fake, concurrency: usize) -> ToolRegistry {
    let client = connect(fake, concurrency).await;
    let defs = client.list_tools().await.expect("tools/list");
    let client = Arc::new(McpClient::Http(client));
    let mut reg = ToolRegistry::new();
    for def in defs {
        reg.register(McpTool::new(client.clone(), "srv", def, &HashMap::new()));
    }
    reg
}

async fn call(reg: &ToolRegistry, tool: &str) -> ToolExecution {
    let call = ToolCall {
        id: "c1".into(),
        name: format!("mcp__srv__{tool}"),
        input: "{}".into(),
        provider_meta: None,
    };
    reg.execute(&call, &SessionId::new("s")).await
}

#[tokio::test]
async fn tools_list_follows_next_cursor_across_pages() {
    let reg = registry(Fake::default(), 4).await;
    let mut names = reg.names();
    names.sort();
    assert_eq!(
        names,
        ["mcp__srv__fail", "mcp__srv__pic", "mcp__srv__pinger"]
    );
}

#[tokio::test]
async fn tools_list_on_a_looping_server_errors_instead_of_hanging() {
    let fake = Fake {
        looping: true,
        ..Fake::default()
    };
    let err = connect(fake, 4).await.list_tools().await.unwrap_err();
    assert!(format!("{err:#}").contains("100 pages"), "{err:#}");
}

#[tokio::test]
async fn is_error_result_sets_the_structured_flag() {
    let reg = registry(Fake::default(), 4).await;
    let out = call(&reg, "fail").await;
    assert!(out.is_error);
    let text = out.content[0].as_text().unwrap();
    assert!(text.contains("quota exceeded"), "{text}");

    // A normal result stays `is_error: false`.
    assert!(!call(&reg, "pic").await.is_error);
}

/// Binary never reaches the context (#727): the image becomes a placeholder
/// line naming mime and size, alongside the text and the resource link.
#[tokio::test]
async fn image_becomes_a_placeholder_and_resource_link_survives() {
    let reg = registry(Fake::default(), 4).await;
    let out = call(&reg, "pic").await;
    assert_eq!(
        out.content,
        [ContentPart::text(
            "chart:\n[binary omitted: image/png, 70 B]\n[resource link: r <https://ex.com/r> (unknown type)]"
        )]
    );
}

/// Endpoint concurrency 1 on purpose: the SSE drain holds the only permit, so
/// a ping reply that regressed into the pool would wait on itself — the
/// timeout turns that deadlock into a failure instead of a hang.
#[tokio::test]
async fn server_ping_on_the_sse_stream_is_answered() {
    let fake = Fake::default();
    let replies = fake.replies.clone();
    let reg = registry(fake, 1).await;
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), call(&reg, "pinger"))
        .await
        .expect("ping reply must not wait on the drain's own endpoint permit");
    assert!(!out.is_error);
    assert_eq!(out.content[0].as_text(), Some("pong"));
    let replies = replies.lock().unwrap().clone();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["result"], json!({}));
    assert!(replies[0]["id"].is_i64(), "{replies:?}");
}
