//! The in-process MCP transport (ADR-0249): an embedder serves an MCP tool
//! surface from its own process — no subprocess, no HTTP self-call — and
//! still reaches the model through the same `mcp` envelope, the scoped
//! connect cache, permission grading and the rhai `mcp::<server>::<tool>`
//! namespace as any other server.
//!
//! Unlike stdio/HTTP, the handler is told **which session** every call comes
//! from (and the call's `request_id`), since an embedder's in-process tools
//! usually authorize per user and tenant. That is also why it only runs
//! inside a session-keyed [`McpScope`](super::McpScope): a connect with no
//! session (the global `mcp:` map, `/mcp add`) is refused.
//!
//! `list_tools` runs once per `(scope key, server)` connect and the result is
//! cached for that key until [`McpScopes::evict_scope`](super::McpScopes::evict_scope)
//! — the embedder keys its scopes by everything that changes the list (user,
//! tenant, permission generation) or evicts when it changes.

use std::fmt;
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use entanglement_core::SessionId;

use super::client::McpToolDef;

/// An MCP server implemented by the embedder, called in-process.
#[async_trait]
pub trait InProcessMcp: Send + Sync {
    /// The tools `session` may see. Called at connect time by the first
    /// session of a scope key; cached per `(scope key, server)`.
    async fn list_tools(&self, session: &SessionId) -> Result<Vec<McpToolDef>>;

    /// One `tools/call`, answered with an MCP `CallToolResult` JSON object
    /// (`content`, optional `isError`/`structuredContent`). `request_id` is
    /// the engine's tool-call id — stable across a resumed turn's retry, so
    /// a write can be made idempotent on it.
    async fn call_tool(
        &self,
        session: &SessionId,
        request_id: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value>;
}

/// A shareable handle to an [`InProcessMcp`] server, carried on
/// [`McpServerConfig::in_process`](super::McpServerConfig::in_process).
/// Equality is identity: two configs are the same server only when they hold
/// the same handler.
#[derive(Clone)]
pub struct InProcessServer(Arc<dyn InProcessMcp>);

impl InProcessServer {
    pub fn new(handler: Arc<dyn InProcessMcp>) -> Self {
        Self(handler)
    }

    pub fn handler(&self) -> &Arc<dyn InProcessMcp> {
        &self.0
    }
}

impl fmt::Debug for InProcessServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InProcessServer(..)")
    }
}

impl PartialEq for InProcessServer {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The connected client side: the server's name (for error text) and its
/// handler.
pub struct InProcessClient {
    server: String,
    handler: Arc<dyn InProcessMcp>,
}

impl InProcessClient {
    pub fn new(server: &str, handle: &InProcessServer) -> Self {
        Self {
            server: server.to_string(),
            handler: handle.0.clone(),
        }
    }

    pub async fn list_tools(&self, session: Option<&SessionId>) -> Result<Vec<McpToolDef>> {
        let Some(session) = session else {
            bail!(no_session(&self.server));
        };
        self.handler.list_tools(session).await
    }

    pub async fn call_tool(
        &self,
        call: Option<(&SessionId, &str)>,
        name: &str,
        arguments: Value,
    ) -> Result<Value> {
        let Some((session, request_id)) = call else {
            bail!(no_session(&self.server));
        };
        self.handler
            .call_tool(session, request_id, name, arguments)
            .await
    }
}

fn no_session(server: &str) -> String {
    format!(
        "MCP server `{server}` is in-process and answers only a session's call — \
         register it in a session's McpScope"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl InProcessMcp for Echo {
        async fn list_tools(&self, session: &SessionId) -> Result<Vec<McpToolDef>> {
            Ok(vec![McpToolDef {
                name: format!("for_{session}"),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }])
        }
        async fn call_tool(
            &self,
            session: &SessionId,
            request_id: &str,
            name: &str,
            arguments: Value,
        ) -> Result<Value> {
            Ok(serde_json::json!({
                "session": session.to_string(), "request_id": request_id,
                "name": name, "arguments": arguments,
            }))
        }
    }

    #[tokio::test]
    async fn the_session_reaches_the_handler() {
        let handle = InProcessServer::new(Arc::new(Echo));
        let client = InProcessClient::new("local", &handle);
        let s = SessionId::new("t:1");
        let tools = client.list_tools(Some(&s)).await.unwrap();
        assert_eq!(tools[0].name, "for_t:1");
        let out = client
            .call_tool(Some((&s, "r7")), "x", serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(out["session"], "t:1");
        assert_eq!(out["request_id"], "r7");
        assert_eq!(out["arguments"]["a"], 1);
    }

    #[tokio::test]
    async fn no_session_is_refused_not_invented() {
        let handle = InProcessServer::new(Arc::new(Echo));
        let client = InProcessClient::new("local", &handle);
        let err = client.list_tools(None).await.unwrap_err().to_string();
        assert!(err.contains("in-process"), "{err}");
        let err = client
            .call_tool(None, "x", Value::Null)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("session"), "{err}");
    }

    #[test]
    fn the_config_is_a_third_exclusive_transport() {
        let mut cfg = super::super::McpServerConfig::from_in_process(Arc::new(Echo));
        assert!(matches!(
            cfg.transport().unwrap(),
            super::super::Transport::InProcess(_)
        ));
        assert_eq!(cfg.target(), "in-process");
        assert_eq!(super::super::transport_label(&cfg), "in-process");
        cfg.url = Some("https://example.com/mcp".into());
        assert!(cfg.transport().is_err());
        // Never written to (or read from) YAML.
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        assert!(!yaml.contains("in_process"), "{yaml}");
    }

    #[test]
    fn equality_is_identity() {
        let a: Arc<dyn InProcessMcp> = Arc::new(Echo);
        assert_eq!(InProcessServer::new(a.clone()), InProcessServer::new(a));
        assert_ne!(
            InProcessServer::new(Arc::new(Echo)),
            InProcessServer::new(Arc::new(Echo))
        );
    }
}
