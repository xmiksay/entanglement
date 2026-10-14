//! Unit tests for [`super::super::scoped`] (#684) — sibling `#[path]` test
//! file, keeping `scoped.rs` under the 400-line cap while private fields stay
//! reachable via the normal descendant-module visibility rule.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use entanglement_core::{SessionId, StoredAuth, TokenStore};
use serde_json::{json, Value};

use super::*;
use crate::mcp::ServerRow;
use crate::tools::{Tool, ToolRegistry};

/// A no-op tool standing in for a global host/MCP tool.
struct FakeTool(&'static str);

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    fn description(&self) -> &str {
        "a fake tool"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn run(&self, _input: &str) -> Result<String> {
        Ok("ok".into())
    }
}

/// A `TokenStore` with nothing in it — the auth-required precheck path.
struct EmptyStore;

impl TokenStore for EmptyStore {
    fn load(&self, _server: &str) -> Result<Option<StoredAuth>> {
        Ok(None)
    }
    fn save(&self, _server: &str, _auth: &StoredAuth) -> Result<()> {
        Ok(())
    }
    fn delete(&self, _server: &str) -> Result<()> {
        Ok(())
    }
}

fn http() -> entanglement_core::HttpClient {
    crate::test_http_client()
}

fn oauth_server() -> McpServerConfig {
    serde_yaml::from_str("url: https://192.0.2.1/mcp\noauth: {}").expect("config")
}

fn scoped_resolver(
    key: &str,
    servers: HashMap<String, McpServerConfig>,
    token_store: Option<Arc<dyn TokenStore>>,
) -> McpScopeResolver {
    let key = key.to_string();
    Arc::new(move |_session| {
        Some(McpScope {
            key: key.clone(),
            servers: servers.clone(),
            token_store: token_store.clone(),
        })
    })
}

fn base_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(FakeTool("read"));
    reg.register(FakeTool("mcp__kb__search"));
    reg
}

fn row_names(rows: &[ServerRow]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

fn seed(scopes: &McpScopes, key: &str, tool: &'static str) {
    let tool: Arc<dyn Tool> = Arc::new(FakeTool(tool));
    let seeded = Arc::new(ScopeServer {
        tools: vec![tool],
        _client: super::super::tool::dead_client(),
    });
    scopes
        .cache
        .lock()
        .unwrap()
        .insert((key.to_string(), "kb".to_string()), seeded);
}

#[tokio::test]
async fn an_unscoped_session_passes_through_untouched() {
    let scopes = McpScopes::new(Arc::new(|_| None), http(), Vec::new());
    let session = SessionId::new("s");

    assert!(scopes.rows(&session).is_none(), "unscoped lists globally");

    let reg = scopes
        .overlay_registry_for_call(&session, base_registry(), "read")
        .await
        .expect("a non-MCP call never connects");
    assert!(reg.contains("read"));
    assert!(reg.contains("mcp__kb__search"));
}

#[tokio::test]
async fn an_unscoped_session_dispatch_overlay_is_identity() {
    let scopes = McpScopes::new(Arc::new(|_| None), http(), Vec::new());
    let session = SessionId::new("s");
    let reg = scopes
        .overlay_registry_for_call(&session, base_registry(), "mcp__kb__search")
        .await
        .expect("unscoped must never fail");
    assert!(reg.contains("mcp__kb__search"));
}

#[tokio::test]
async fn a_scoped_session_owns_its_whole_mcp_namespace() {
    // The scope declares a `kb` of its own (not connected yet) — the *global*
    // `mcp__kb__search` must vanish from the registry, `list_mcp` shows the
    // scope's server only, and non-MCP tools survive.
    let servers = HashMap::from([("kb".to_string(), oauth_server())]);
    let scopes = McpScopes::new(scoped_resolver("user-a", servers, None), http(), Vec::new());
    let session = SessionId::new("s");

    let rows = scopes.rows(&session).expect("scoped");
    assert_eq!(row_names(&rows), vec!["kb"]);
    assert!(!rows[0].connected && rows[0].tools.is_empty());
    assert!(rows[0].auth_required, "OAuth with no stored credential");

    let reg = scopes
        .overlay_registry_for_call(&session, base_registry(), "read")
        .await
        .expect("a non-MCP call never connects");
    assert!(reg.contains("read"));
    assert!(!reg.contains("mcp__kb__search"));
}

#[tokio::test]
async fn an_oauth_server_with_no_stored_credential_is_auth_required() {
    for token_store in [None, Some(Arc::new(EmptyStore) as Arc<dyn TokenStore>)] {
        let servers = HashMap::from([("kb".to_string(), oauth_server())]);
        let scopes = McpScopes::new(
            scoped_resolver("user-a", servers, token_store),
            http(),
            Vec::new(),
        );
        let session = SessionId::new("s");
        let err = scopes
            .overlay_registry_for_call(&session, base_registry(), "mcp__kb__search")
            .await
            .err()
            .expect("no credential must be a tool error, not a connect attempt");
        assert!(err.contains("`kb`"), "{err}");
        assert!(
            err.contains("requires authorization for this user"),
            "{err}"
        );
    }
}

#[tokio::test]
async fn a_call_to_a_tool_outside_the_scope_never_connects() {
    // The named tool belongs to no scope server: no connect is attempted (the
    // OAuth precheck would have errored), and the overlay still strips the
    // global MCP namespace.
    let servers = HashMap::from([("kb".to_string(), oauth_server())]);
    let scopes = McpScopes::new(scoped_resolver("user-a", servers, None), http(), Vec::new());
    let session = SessionId::new("s");
    let reg = scopes
        .overlay_registry_for_call(&session, base_registry(), "read")
        .await
        .expect("a non-MCP tool passes through");
    assert!(reg.contains("read"));
    assert!(!reg.contains("mcp__kb__search"));
}

#[test]
fn owning_server_matches_by_the_sanitized_prefix() {
    let scope = McpScope {
        key: "k".into(),
        servers: HashMap::from([("my server".to_string(), oauth_server())]),
        token_store: None,
    };
    // `McpTool` advertises under the sanitized name, so the reverse match must
    // sanitize too.
    assert_eq!(
        owning_server(&scope, "mcp__my_server__read_file"),
        Some("my server".to_string())
    );
    assert_eq!(owning_server(&scope, "mcp__other__x"), None);
    assert_eq!(owning_server(&scope, "read"), None);
}

#[tokio::test]
async fn cached_scope_tools_are_listed_and_dispatched_and_evicted() {
    let servers = HashMap::from([("kb".to_string(), oauth_server())]);
    let scopes = McpScopes::new(scoped_resolver("user-a", servers, None), http(), Vec::new());
    let session = SessionId::new("s");

    // Seed the cache directly (unit scope — the integration suite covers a
    // real connect).
    seed(&scopes, "user-a", "mcp__kb__lookup");
    let rows = scopes.rows(&session).expect("scoped");
    assert!(rows[0].connected && !rows[0].auth_required);
    assert_eq!(rows[0].tools, vec!["mcp__kb__lookup".to_string()]);

    let reg = scopes
        .overlay_registry_for_call(&session, base_registry(), "read")
        .await
        .expect("a non-MCP call never connects");
    assert!(reg.contains("mcp__kb__lookup"));
    assert!(!reg.contains("mcp__kb__search"));

    scopes.evict_scope("user-a");
    let rows = scopes.rows(&session).expect("scoped");
    assert!(!rows[0].connected && rows[0].tools.is_empty());
}

#[tokio::test]
async fn scopes_with_different_keys_do_not_see_each_others_cache() {
    let servers = HashMap::from([("kb".to_string(), oauth_server())]);
    let scopes = McpScopes::new(
        scoped_resolver("user-b", servers, None), // sessions resolve to user-b
        http(),
        Vec::new(),
    );
    let session = SessionId::new("s");

    // user-a's cached `kb` must be invisible to a user-b session.
    seed(&scopes, "user-a", "mcp__kb__lookup");
    let rows = scopes.rows(&session).expect("scoped");
    assert!(!rows[0].connected && rows[0].tools.is_empty());
    let reg = scopes
        .overlay_registry_for_call(&session, base_registry(), "read")
        .await
        .expect("a non-MCP call never connects");
    assert!(!reg.contains("mcp__kb__lookup"));
}

/// ADR-0226's clash rule holds inside a scope too: of two servers whose tool
/// names would collide (`my.docs` / `my_docs`), only the first by name is
/// usable — listed, owning tools, connectable.
#[tokio::test]
async fn a_clashing_server_in_one_scope_is_skipped() {
    let servers = HashMap::from([
        ("my.docs".to_string(), oauth_server()),
        ("my_docs".to_string(), oauth_server()),
        ("kb".to_string(), oauth_server()),
    ]);
    let scopes = McpScopes::new(scoped_resolver("user-a", servers, None), http(), Vec::new());
    let session = SessionId::new("s");
    let rows = scopes.rows(&session).expect("scoped");
    assert_eq!(row_names(&rows), vec!["kb", "my.docs"]);
    let scope = scopes.scope_of(&session).unwrap();
    assert_eq!(
        owning_server(&scope, "mcp__my_docs__read").as_deref(),
        Some("my.docs")
    );
    let (_, skipped) = usable_servers(&scope);
    assert_eq!(skipped, vec![("my_docs", "my.docs")]);
}
