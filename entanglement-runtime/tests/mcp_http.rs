//! Streamable-HTTP MCP transport end-to-end (#312, ADR-0080).
//!
//! Spins up a minimal `POST /mcp` server (the shape the site exposes) and drives
//! the real [`HttpClient`] through `initialize` → `tools/list` → `tools/call`,
//! asserting the `Authorization` header authenticates every request, the
//! `Mcp-Session-Id` issued on `initialize` is echoed back, and both response
//! shapes the spec allows — a lone JSON body and an SSE stream — are handled.
//!
//! Needs both `mcp-http` (the transport under test) and `serve` (axum, the test
//! server) — the default feature set enables both.
#![cfg(all(feature = "mcp-http", feature = "serve"))]

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use entanglement_runtime::mcp::HttpClient;

const SESSION_ID: &str = "sess-abc123";
const TOKEN: &str = "Bearer sekret-token";

/// The `initialize` reply carries the session id; `tools/list` answers over SSE;
/// `tools/call` answers with a lone JSON body — so one run exercises both shapes.
/// Every non-handshake request must present the auth header and the session id.
async fn mcp(State(()): State<()>, headers: HeaderMap, Json(req): Json<Value>) -> Response {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let session = headers
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Every request past the handshake must carry the token.
    let is_handshake = method == "initialize" || method == "notifications/initialized";
    if !is_handshake && auth != TOKEN {
        return (StatusCode::UNAUTHORIZED, "missing token").into_response();
    }

    match method {
        "initialize" => {
            let body = json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "protocolVersion": "2025-03-26", "serverInfo": { "name": "test" } }
            });
            ([("Mcp-Session-Id", SESSION_ID)], Json(body)).into_response()
        }
        // Fire-and-forget ack.
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => {
            // Answer over SSE to exercise the streaming path.
            let resp = json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": [
                    { "name": "ping", "description": "pong it", "inputSchema": { "type": "object", "properties": {} } }
                ] }
            });
            let sse = format!(
                "event: message\ndata: {}\n\n",
                serde_json::to_string(&resp).unwrap()
            );
            ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
        "tools/call" => {
            // Echo the session id back so the test can assert the round-trip.
            let text = format!("session={session}");
            let body = json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "content": [ { "type": "text", "text": text } ] }
            });
            Json(body).into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "unknown method").into_response(),
    }
}

async fn spawn_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route("/mcp", post(mcp)).with_state(());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}/mcp")
}

#[tokio::test]
async fn http_transport_lists_and_calls_with_auth() {
    let url = spawn_server().await;
    let headers = HashMap::from([("Authorization".to_string(), TOKEN.to_string())]);

    let client = HttpClient::connect("test", &url, &headers)
        .await
        .expect("handshake");

    // `tools/list` over SSE.
    let tools = client.list_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "ping");
    assert_eq!(tools[0].description, "pong it");

    // `tools/call` over JSON — the server echoes the session id it received,
    // proving the `Mcp-Session-Id` handed out on `initialize` round-tripped.
    let res = client
        .call_tool("ping", json!({}))
        .await
        .expect("tools/call");
    let text = res["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, format!("session={SESSION_ID}"));
}

#[tokio::test]
async fn missing_token_is_rejected() {
    let url = spawn_server().await;
    // No Authorization header → the server 401s `tools/list`; the client surfaces
    // it as an error rather than hanging. (`initialize` is allowed through.)
    let client = HttpClient::connect("test", &url, &HashMap::new())
        .await
        .expect("handshake");
    let err = client.list_tools().await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "got: {err:#}");
}
