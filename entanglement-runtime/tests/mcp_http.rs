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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use entanglement_runtime::mcp::{connect, AvailableServer, HttpClient, McpServerConfig};
use entanglement_runtime::tools::ToolRegistry;
// The shared per-endpoint pool (#559) — distinct from `mcp::HttpClient` above,
// which is the MCP *transport*'s historical name (ADR-0153).
use entanglement_provider::{HttpClient as PoolHttpClient, RetryConfig};

const SESSION_ID: &str = "sess-abc123";
const TOKEN: &str = "Bearer sekret-token";

/// Every test here now drives its MCP calls through the shared endpoint pool
/// (#559), which — without this — would write real `.state`/`.lock` files
/// into the developer's actual `${data_dir}/entanglement/endpoints/`
/// (mirrors `entanglement-provider/tests/streaming.rs`'s isolation guard).
fn ensure_shared_state_disabled() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        std::env::set_var("ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE", "1");
    });
}

fn test_http_client() -> PoolHttpClient {
    ensure_shared_state_disabled();
    PoolHttpClient::new().unwrap()
}

fn test_http_client_with(config: RetryConfig) -> PoolHttpClient {
    ensure_shared_state_disabled();
    PoolHttpClient::with_config(config).unwrap()
}

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

    let client = HttpClient::connect("test", &url, &headers, test_http_client(), None, None)
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
    let client = HttpClient::connect(
        "test",
        &url,
        &HashMap::new(),
        test_http_client(),
        None,
        None,
    )
    .await
    .expect("handshake");
    let err = client.list_tools().await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "got: {err:#}");
}

// ── #559: MCP traffic now shares the endpoint pool's concurrency cap ───────

/// Tracks how many `tools/call` requests are in flight at once, so the test
/// below can prove a concurrency-1 pool genuinely serializes MCP calls —
/// before #559 this was structurally impossible, since `McpHttpClient` built
/// its own bare `reqwest::Client` with no concurrency gate at all.
#[derive(Clone, Default)]
struct ConcurrencyTracker {
    in_flight: Arc<AtomicUsize>,
    max_seen: Arc<AtomicUsize>,
}

async fn mcp_slow_call(
    State(tracker): State<ConcurrencyTracker>,
    Json(req): Json<Value>,
) -> Response {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();
    match method {
        "initialize" => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "protocolVersion": "2025-03-26", "serverInfo": { "name": "test" } }
        }))
        .into_response(),
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/call" => {
            let now = tracker.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            tracker.max_seen.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            tracker.in_flight.fetch_sub(1, Ordering::SeqCst);
            Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "content": [ { "type": "text", "text": "ok" } ] }
            }))
            .into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "unknown method").into_response(),
    }
}

async fn spawn_slow_server() -> (String, ConcurrencyTracker) {
    let tracker = ConcurrencyTracker::default();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/mcp", post(mcp_slow_call))
        .with_state(tracker.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}/mcp"), tracker)
}

#[tokio::test]
async fn mcp_calls_are_serialized_by_the_endpoint_concurrency_cap() {
    let (url, tracker) = spawn_slow_server().await;
    let http = test_http_client_with(RetryConfig {
        concurrency: 1,
        ..RetryConfig::default()
    });
    let client = HttpClient::connect("test", &url, &HashMap::new(), http, None, None)
        .await
        .expect("handshake");

    let (a, b) = tokio::join!(
        client.call_tool("ping", json!({})),
        client.call_tool("ping", json!({})),
    );
    a.expect("tools/call a");
    b.expect("tools/call b");

    assert_eq!(
        tracker.max_seen.load(Ordering::SeqCst),
        1,
        "two concurrent MCP calls through a concurrency-1 pool must never run \
         simultaneously — the endpoint's concurrency permit is the guard"
    );
}

// ── #660: startup connect fails fast and fans out concurrently ─────────────

fn http_server_cfg(url: &str) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        oauth: None,
        disabled: false,
        capabilities: HashMap::new(),
        state: None,
    }
}

fn available(config: McpServerConfig) -> AvailableServer {
    AvailableServer {
        config,
        key_env: None,
        provider: None,
    }
}

/// Effectively no RPM pacing gap between requests — isolates the timing
/// assertions below from the endpoint pool's *default* 50-RPM baseline
/// spacing (1.2s between any two calls to the same endpoint, unrelated to
/// #660's retry-ladder/concurrency fix) so they measure only what this fix
/// changes.
fn unpaced_http_client() -> PoolHttpClient {
    test_http_client_with(RetryConfig {
        rpm: 1_000_000,
        ..RetryConfig::default()
    })
}

/// A bound-then-dropped listener: nothing answers on this port, so a connect
/// attempt gets an immediate OS-level `ECONNREFUSED` — a deterministic
/// stand-in for "the configured MCP server just isn't running" (#660).
async fn closed_port_url() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/mcp")
}

#[tokio::test]
async fn startup_connect_fails_fast_on_a_closed_port() {
    let url = closed_port_url().await;
    let servers = HashMap::from([("dead".to_string(), available(http_server_cfg(&url)))]);
    let mut registry = ToolRegistry::new();
    let http = unpaced_http_client();

    let started = Instant::now();
    let active = connect(&servers, &mut registry, &[], &http).await;
    let elapsed = started.elapsed();

    assert!(active.is_empty());
    assert!(registry.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "a closed-port MCP server must fail fast, not ride the LLM-tuned retry \
         ladder (5 attempts, ~10s) — took {elapsed:?}"
    );
}

#[tokio::test]
async fn startup_connect_runs_servers_concurrently_and_a_healthy_one_still_connects() {
    let dead_a = closed_port_url().await;
    let dead_b = closed_port_url().await;
    let healthy_url = spawn_server().await;
    // `spawn_server`'s handler 401s any non-handshake call without the token
    // (see `mcp` above), so the healthy entry needs the auth header the dead
    // ones don't.
    let mut healthy_cfg = http_server_cfg(&healthy_url);
    healthy_cfg
        .headers
        .insert("Authorization".to_string(), TOKEN.to_string());
    let servers = HashMap::from([
        ("dead-a".to_string(), available(http_server_cfg(&dead_a))),
        ("dead-b".to_string(), available(http_server_cfg(&dead_b))),
        ("healthy".to_string(), available(healthy_cfg)),
    ]);
    let mut registry = ToolRegistry::new();
    let http = unpaced_http_client();

    let started = Instant::now();
    let active = connect(&servers, &mut registry, &[], &http).await;
    let elapsed = started.elapsed();

    assert_eq!(active.len(), 1, "only the healthy server should connect");
    assert!(active.contains_key("healthy"));
    assert_eq!(registry.specs().len(), 1);
    assert!(
        elapsed < Duration::from_secs(2),
        "two dead servers connecting concurrently (not one after another) plus \
         one healthy connect must land well under the old sequential cost of \
         ~10s per dead server — took {elapsed:?}"
    );
}
