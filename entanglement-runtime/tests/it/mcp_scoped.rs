//! Session-keyed per-user MCP scopes end-to-end (#684): two users with
//! same-named servers hit their own endpoints with their own bearer tokens,
//! connections are cached once per `(scope, server)`, `list_mcp`/`mcp_tools`
//! resolve through the scope (a scoped session never sees a global server,
//! ADR-0188 + ADR-0226), an unauthorized scope fails as auth-required, and
//! `evict_scope`
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
use entanglement_runtime::discover;
use entanglement_runtime::mcp::{
    ActiveServers, AvailableMcp, McpScope, McpScopeResolver, McpScopes, McpServerConfig,
};
use entanglement_runtime::tool_advertising::DeliveredSchemas;
use entanglement_runtime::tools::ToolRegistry;

use entanglement_provider::HttpClient as PoolHttpClient;

fn test_http_client() -> PoolHttpClient {
    // Mirrors `mcp_http.rs`: never write shared endpoint state from tests.
    PoolHttpClient::with_shared_state(
        entanglement_core::SharedStateLocation::Disabled,
        entanglement_core::RetryConfig::default(),
    )
    .unwrap()
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
        description: Some("the user's knowledge base".into()),
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
            &HashMap::new(),
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
async fn prewarm_connects_and_list_rows_show_the_scope_tools() {
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
    let before = scopes.rows(&session).expect("scoped");
    assert_eq!(before.len(), 1);
    assert!(!before[0].connected && before[0].tools.is_empty());
    assert!(!before[0].auth_required, "a credential is stored");

    let failures = scopes.prewarm(&session).await;
    assert!(failures.is_empty(), "{failures:?}");
    let after = scopes.rows(&session).expect("scoped");
    assert!(after[0].connected);
    assert_eq!(after[0].tools, vec!["mcp__kb__search".to_string()]);
}

/// ADR-0188 scoping for the discovery pair: a scoped session's `list_mcp`
/// shows its scope's `kb` and never the global roster's server, `mcp_tools`
/// reads (connecting) the scope's `kb`, and a global server is unknown to it.
#[tokio::test]
async fn list_mcp_and_mcp_tools_resolve_through_the_scope() {
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
    let avail = AvailableMcp::default();
    let global: McpServerConfig =
        serde_yaml::from_str("url: https://192.0.2.1/global").expect("config");
    avail.insert_user("globalsrv", global);
    let active: ActiveServers = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let registry = ToolRegistry::new().shared();
    let delivered = std::sync::Mutex::new(DeliveredSchemas::new());
    let http = test_http_client();
    let src = discover::McpSources {
        avail: &avail,
        active: &active,
        registry: &registry,
        scopes: Some(&scopes),
        http: Some(&http),
        delivered: &delivered,
    };

    let listed = discover::list_mcp(&src, &session);
    assert!(
        listed.contains("kb — the user's knowledge base [available]"),
        "{listed}"
    );
    assert!(!listed.contains("globalsrv"), "{listed}");

    let (tools, is_error) = discover::mcp_tools(&src, &session, r#"{"name":"kb"}"#).await;
    assert!(!is_error, "{tools}");
    assert!(tools.contains("- search:"), "{tools}");
    assert!(delivered
        .lock()
        .unwrap()
        .contains(&session, "mcp__kb__search"));

    let (unknown, is_error) = discover::mcp_tools(&src, &session, r#"{"name":"globalsrv"}"#).await;
    assert!(is_error);
    assert!(
        unknown.contains("no MCP server named `globalsrv`"),
        "{unknown}"
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

// ── ADR-0188 isolation at dispatch: a scoped session never reaches a global
// server, whether or not that server is connected for the process. ─────────

struct ScriptedLlm(std::sync::Mutex<Vec<entanglement_core::LlmResponse>>);

#[async_trait::async_trait]
impl entanglement_core::Llm for ScriptedLlm {
    async fn stream(
        &mut self,
        _req: entanglement_core::LlmRequest<'_>,
    ) -> anyhow::Result<entanglement_core::LlmStream> {
        let resp = self.0.lock().unwrap().pop().unwrap_or_default();
        Ok(entanglement_core::stream_from_response(resp))
    }
}

/// A stand-in for a global server's already-registered tool.
struct GlobalSearch;

#[async_trait::async_trait]
impl entanglement_runtime::Tool for GlobalSearch {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("mcp__globalsrv__search")
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        Ok("GLOBAL REACHED".into())
    }
}

/// One `mcp__globalsrv__search` call from a session scoped to user-a's `kb`,
/// with `globalsrv` configured globally at `global_url` and, when
/// `registered`, already connected (its tool in the global registry).
async fn scoped_call_to_a_global_server(global_url: &str, registered: bool) -> (String, bool) {
    use entanglement_core::{EngineConfig, Holly, InMsg, LlmResponse, OutEvent};
    use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};

    let (kb_url, _log) = spawn_server().await;
    let store = Arc::new(InMemoryUserTokenStore::new());
    store
        .save(&UserId("user-a".into()), "kb", &stored_auth("tok-a"))
        .unwrap();
    let session = SessionId::new("s");
    let scopes = McpScopes::new(
        embedder_resolver(
            store,
            HashMap::from([(session.clone(), ("user-a".to_string(), kb_url))]),
        ),
        test_http_client(),
        Vec::new(),
    );
    let avail = AvailableMcp::default();
    let global: McpServerConfig =
        serde_yaml::from_str(&format!("url: {global_url}")).expect("config");
    avail.insert_user("globalsrv", global);
    let mut reg = ToolRegistry::new();
    if registered {
        reg.register(GlobalSearch);
    }
    let script = vec![
        LlmResponse::default(),
        LlmResponse {
            tool_calls: vec![call("mcp__globalsrv__search")],
            ..Default::default()
        },
    ];
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm(std::sync::Mutex::new(script.clone())))
                as Box<dyn entanglement_core::Llm>
        }),
        ..EngineConfig::default()
    });
    let _executor = ToolExecutor::builder(&holly, reg.shared())
        .perm_modes(crate::mode_support::perm_modes())
        .mode_table(crate::mode_support::allow_all_table())
        .mcp_scopes(scopes)
        .discovery(DiscoverySurface {
            mcp_avail: Arc::new(avail),
            http: Some(test_http_client()),
            ..Default::default()
        })
        .spawn();
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(session.clone(), "go"))
        .await
        .unwrap();
    loop {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), sub.recv())
            .await
            .expect("timed out")
            .unwrap();
        if let OutEvent::ToolOutput {
            output, is_error, ..
        } = ev
        {
            return (output, is_error);
        }
    }
}

#[tokio::test]
async fn a_scoped_session_cannot_reach_a_global_server_not_yet_connected() {
    let (global_url, global_log) = spawn_server().await;
    let (output, is_error) = scoped_call_to_a_global_server(&global_url, false).await;
    assert!(is_error, "{output}");
    assert!(
        output.contains("no MCP server named `globalsrv`"),
        "{output}"
    );
    assert_eq!(
        global_log.initializes.load(Ordering::SeqCst),
        0,
        "the global server must never be connected for a scoped session"
    );
}

#[tokio::test]
async fn a_scoped_session_cannot_reach_an_already_connected_global_server() {
    let (output, is_error) = scoped_call_to_a_global_server("http://127.0.0.1:1/mcp", true).await;
    assert!(is_error, "{output}");
    assert!(!output.contains("GLOBAL REACHED"), "{output}");
    assert!(
        output.contains("no MCP server named `globalsrv`"),
        "{output}"
    );
}

/// N1: a `disabled` scoped server is unreachable, like `list_mcp` never
/// lists it — no connect, a tool error instead.
#[tokio::test]
async fn a_disabled_scoped_server_is_unreachable() {
    let (url, log) = spawn_server().await;
    let mut cfg = oauth_http_server(&url);
    cfg.disabled = true;
    let session = SessionId::new("s");
    let scopes = McpScopes::new(
        Arc::new(move |_session| {
            Some(McpScope {
                key: "user-a".into(),
                servers: HashMap::from([("kb".to_string(), cfg.clone())]),
                token_store: None,
            })
        }),
        test_http_client(),
        Vec::new(),
    );
    assert!(scopes.rows(&session).unwrap().is_empty());
    assert!(scopes.prewarm(&session).await.is_empty());
    let reg = scopes
        .overlay_registry_for_call(&session, ToolRegistry::new(), "mcp__kb__search")
        .await
        .expect("no owning server, nothing to connect");
    let decline = scopes.scope_decline(&session, &reg, "mcp__kb__search");
    assert!(
        decline
            .as_deref()
            .unwrap_or("")
            .contains("no MCP server named `kb`"),
        "{decline:?}"
    );
    assert_eq!(log.initializes.load(Ordering::SeqCst), 0);
}
