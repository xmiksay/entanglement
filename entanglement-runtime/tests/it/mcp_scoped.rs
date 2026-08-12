//! Session-keyed per-user MCP scopes end-to-end (#684): two users with
//! same-named servers hit their own endpoints with their own bearer tokens,
//! connections are cached once per `(scope, server)`, advertisement follows
//! `prewarm`, an unauthorized scope fails as auth-required, and `evict_scope`
//! forces a fresh connect. The credential side runs through the real ADR-0184
//! seam — `InMemoryUserTokenStore` + `user_scoped` — exactly as an embedder
//! wires it.
//!
//! Needs `mcp-http` (the transport) and `serve` (axum, the fake server) —
//! the default feature set enables both.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use entanglement_core::{content_text, SessionId, StoredAuth, TokenSet, ToolCall};
use entanglement_provider::{user_scoped, InMemoryUserTokenStore, UserId, UserTokenStore};
use entanglement_runtime::mcp::{McpScope, McpScopeResolver, McpScopes, McpServerConfig};
use entanglement_runtime::tools::ToolRegistry;

use entanglement_provider::HttpClient as PoolHttpClient;

fn test_http_client() -> PoolHttpClient {
    // Mirrors `mcp_http.rs`: never write shared endpoint state from tests.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        std::env::set_var("ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE", "1");
    });
    PoolHttpClient::new().unwrap()
}

/// Per-server counters: how many `initialize` handshakes ran, and which bearer
/// tokens `tools/call` saw — the assertions read the *server's* view, not the
/// client's.
#[derive(Clone, Default)]
struct ServerLog {
    initializes: Arc<AtomicUsize>,
    call_bearers: Arc<std::sync::Mutex<Vec<String>>>,
}

/// A minimal streamable-HTTP MCP server advertising one `search` tool whose
/// call echoes the presented bearer token.
async fn mcp(State(log): State<ServerLog>, headers: HeaderMap, Json(req): Json<Value>) -> Response {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    match method {
        "initialize" => {
            log.initializes.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "protocolVersion": "2025-03-26", "serverInfo": { "name": "test" } }
            }))
            .into_response()
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "tools": [
                { "name": "search", "description": "find things",
                  "inputSchema": { "type": "object", "properties": {} } }
            ] }
        }))
        .into_response(),
        "tools/call" => {
            log.call_bearers.lock().unwrap().push(auth.clone());
            Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "content": [ { "type": "text", "text": format!("bearer={auth}") } ] }
            }))
            .into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "unknown method").into_response(),
    }
}

async fn spawn_server() -> (String, ServerLog) {
    let log = ServerLog::default();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/mcp", post(mcp))
        .with_state(log.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}/mcp"), log)
}

fn oauth_http_server(url: &str) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        oauth: Some(Default::default()),
        disabled: false,
        capabilities: HashMap::new(),
        state: None,
    }
}

fn stored_auth(access_token: &str) -> StoredAuth {
    StoredAuth {
        client_id: "cid".into(),
        client_secret: None,
        token_endpoint: "https://192.0.2.1/token".into(),
        revocation_endpoint: None,
        resource: None,
        tokens: TokenSet {
            access_token: access_token.into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            // No expiry recorded → never proactively refreshed, so the dead
            // token endpoint above is never contacted.
            expires_at: None,
            scope: None,
        },
    }
}

/// The embedder recipe under test: one shared `UserTokenStore`, a
/// session→user map of the embedder's own, and a resolver closing over both —
/// each user's scope carries the same server *name* pointing at their own URL.
fn embedder_resolver(
    store: Arc<InMemoryUserTokenStore>,
    session_users: HashMap<SessionId, (String, String)>, // session → (user, url)
) -> McpScopeResolver {
    Arc::new(move |session| {
        let (user, url) = session_users.get(session)?;
        Some(McpScope {
            key: user.clone(),
            servers: HashMap::from([("kb".to_string(), oauth_http_server(url))]),
            token_store: Some(user_scoped(store.clone(), UserId(user.clone()))),
        })
    })
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        name: name.into(),
        input: "{}".into(),
        provider_meta: None,
    }
}

#[tokio::test]
async fn two_scopes_with_the_same_server_name_use_their_own_endpoint_and_bearer() {
    let (url_a, log_a) = spawn_server().await;
    let (url_b, log_b) = spawn_server().await;
    let (url_global, log_global) = spawn_server().await;

    let store = Arc::new(InMemoryUserTokenStore::new());
    store
        .save(&UserId("user-a".into()), "kb", &stored_auth("tok-a"))
        .unwrap();
    store
        .save(&UserId("user-b".into()), "kb", &stored_auth("tok-b"))
        .unwrap();

    let session_a = SessionId::new("sa");
    let session_b = SessionId::new("sb");
    let scopes = McpScopes::new(
        embedder_resolver(
            store,
            HashMap::from([
                (session_a.clone(), ("user-a".to_string(), url_a)),
                (session_b.clone(), ("user-b".to_string(), url_b)),
            ]),
        ),
        test_http_client(),
        Vec::new(),
    );

    // The global registry has its own `kb` — a third endpoint scoped sessions
    // must never touch.
    let mut base = ToolRegistry::new();
    let global_client = entanglement_runtime::mcp::HttpClient::connect(
        "kb",
        &url_global,
        &HashMap::new(),
        test_http_client(),
        None,
        None,
    )
    .await
    .expect("global handshake");
    let defs = global_client.list_tools().await.expect("global tools/list");
    let global_client = Arc::new(entanglement_runtime::mcp::McpClient::Http(global_client));
    for def in defs {
        base.register(entanglement_runtime::mcp::McpTool::new(
            global_client.clone(),
            "kb",
            def,
        ));
    }
    assert!(base.contains("mcp__kb__search"));

    for (session, expected) in [
        (&session_a, "bearer=Bearer tok-a"),
        (&session_b, "bearer=Bearer tok-b"),
    ] {
        let reg = scopes
            .overlay_registry_for_call(session, base.clone(), "mcp__kb__search")
            .await
            .expect("scoped connect");
        let result = reg.execute(&call("mcp__kb__search"), session).await;
        assert!(!result.is_error, "{:?}", content_text(&result.content));
        assert_eq!(content_text(&result.content), expected);
    }

    assert_eq!(
        log_a.call_bearers.lock().unwrap().as_slice(),
        ["Bearer tok-a"]
    );
    assert_eq!(
        log_b.call_bearers.lock().unwrap().as_slice(),
        ["Bearer tok-b"]
    );
    assert!(
        log_global.call_bearers.lock().unwrap().is_empty(),
        "a scoped session must never reach the global `kb`"
    );
}

#[tokio::test]
async fn one_scope_connects_once_across_sessions_and_concurrent_first_calls() {
    let (url, log) = spawn_server().await;
    let store = Arc::new(InMemoryUserTokenStore::new());
    store
        .save(&UserId("user-a".into()), "kb", &stored_auth("tok-a"))
        .unwrap();

    let s1 = SessionId::new("s1");
    let s2 = SessionId::new("s2");
    let scopes = McpScopes::new(
        embedder_resolver(
            store,
            HashMap::from([
                (s1.clone(), ("user-a".to_string(), url.clone())),
                (s2.clone(), ("user-a".to_string(), url)),
            ]),
        ),
        test_http_client(),
        Vec::new(),
    );

    // Two sessions of the same scope racing their first call: the #556-style
    // connect guard must collapse them into one handshake.
    let (ra, rb) = tokio::join!(
        scopes.overlay_registry_for_call(&s1, ToolRegistry::new(), "mcp__kb__search"),
        scopes.overlay_registry_for_call(&s2, ToolRegistry::new(), "mcp__kb__search"),
    );
    ra.expect("first call connects");
    rb.expect("second call reuses");
    assert_eq!(log.initializes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn prewarm_populates_the_advertised_specs() {
    let (url, _log) = spawn_server().await;
    let store = Arc::new(InMemoryUserTokenStore::new());
    store
        .save(&UserId("user-a".into()), "kb", &stored_auth("tok-a"))
        .unwrap();

    let session = SessionId::new("s");
    let scopes = McpScopes::new(
        embedder_resolver(
            store,
            HashMap::from([(session.clone(), ("user-a".to_string(), url))]),
        ),
        test_http_client(),
        Vec::new(),
    );

    // Global specs carry a `read` host tool and an `mcp__other__x` the scope
    // must strip.
    let global_specs = vec![
        entanglement_core::ToolSpec::new("read", "read a file"),
        entanglement_core::ToolSpec::new("mcp__other__x", "someone else's"),
    ];

    // Before prewarm: the scope owns the MCP namespace but has nothing
    // connected yet — no MCP tools advertised at all.
    let before = scopes.overlay_specs(&session, global_specs.clone());
    assert_eq!(
        before.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["read"]
    );

    let failures = scopes.prewarm(&session).await;
    assert!(failures.is_empty(), "{failures:?}");

    let after = scopes.overlay_specs(&session, global_specs);
    assert_eq!(
        after.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["mcp__kb__search", "read"]
    );
}

#[tokio::test]
async fn a_scope_without_a_stored_credential_fails_as_auth_required() {
    let (url, log) = spawn_server().await;
    // The store knows user-b, but this session belongs to user-a — whose slice
    // is empty.
    let store = Arc::new(InMemoryUserTokenStore::new());
    store
        .save(&UserId("user-b".into()), "kb", &stored_auth("tok-b"))
        .unwrap();

    let session = SessionId::new("s");
    let scopes = McpScopes::new(
        embedder_resolver(
            store,
            HashMap::from([(session.clone(), ("user-a".to_string(), url))]),
        ),
        test_http_client(),
        Vec::new(),
    );

    let err = scopes
        .overlay_registry_for_call(&session, ToolRegistry::new(), "mcp__kb__search")
        .await
        .err()
        .expect("no credential for this user's slice");
    assert!(err.contains("`kb`"), "{err}");
    assert!(
        err.contains("requires authorization for this user"),
        "{err}"
    );
    assert_eq!(
        log.initializes.load(Ordering::SeqCst),
        0,
        "the refusal must precede any connect attempt"
    );
}

#[tokio::test]
async fn evict_scope_drops_the_connection_and_the_next_call_reconnects() {
    let (url, log) = spawn_server().await;
    let store = Arc::new(InMemoryUserTokenStore::new());
    store
        .save(&UserId("user-a".into()), "kb", &stored_auth("tok-a"))
        .unwrap();

    let session = SessionId::new("s");
    let scopes = McpScopes::new(
        embedder_resolver(
            store,
            HashMap::from([(session.clone(), ("user-a".to_string(), url))]),
        ),
        test_http_client(),
        Vec::new(),
    );

    scopes
        .overlay_registry_for_call(&session, ToolRegistry::new(), "mcp__kb__search")
        .await
        .expect("first connect");
    assert_eq!(log.initializes.load(Ordering::SeqCst), 1);

    scopes.evict_scope("user-a");
    scopes
        .overlay_registry_for_call(&session, ToolRegistry::new(), "mcp__kb__search")
        .await
        .expect("reconnect after evict");
    assert_eq!(log.initializes.load(Ordering::SeqCst), 2);
}
