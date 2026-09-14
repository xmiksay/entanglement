//! The `mcp_enable` host tool (#542): the agent's own handle on the
//! `allowed` tier of the three-state MCP model — mirroring `load_skill`'s
//! shape (a real registry tool, profile-graded like any other, no
//! runtime-executor interception).
//!
//! The schema is **static** (`server: string`) in every advertising mode
//! (#560, ADR-0196 §4/§6): no live per-session `enum` of the currently
//! available servers — that would be a second cache seam ADR-0192 didn't
//! close (the schema itself would still busts a session's advertised-array
//! cache whenever a new server unlocks). The live roster lives in
//! `explore`/`describe` instead; an unknown/not-yet-unlocked `server` name is
//! just a normal tool error, closest-match-hinted like any other.
//!
//! Enabling connects the server lazily and scopes its tools' visibility to
//! the calling session (`AvailableMcp::mark_enabled` + the runtime's spec
//! filter). Nothing persists — the enablement dies with the session, and
//! `config.yml` is never written (unlike `/mcp add`). Note the newly
//! registered tools reach the model on its *next* round (the spec snapshot
//! is per round); the tool's reply says so, and points at `explore` to list
//! them by name.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use entanglement_core::{ContentPart, SessionId};
use serde::Deserialize;

use crate::tools::{text_parts, SharedRegistry, Tool, WeakRegistry};

use super::available::{enable_for_session, AvailableMcp};
use super::live::ActiveServers;

/// See the module docs. Holds a [`WeakRegistry`] back-reference (#556): this
/// tool is itself registered *into* the registry it needs to call back into
/// (to register a lazily-connected server's tools), so an owning `Arc` here
/// would form a reference cycle — the `ToolRegistry`, and every `Arc<McpClient>`
/// it holds (with their `kill_on_drop` stdio children), would never drop.
pub struct McpEnableTool {
    avail: Arc<AvailableMcp>,
    registry: WeakRegistry,
    active: ActiveServers,
    http: entanglement_core::HttpClient,
}

#[derive(Deserialize)]
struct Input {
    server: String,
}

impl McpEnableTool {
    pub fn new(
        avail: Arc<AvailableMcp>,
        registry: &SharedRegistry,
        active: ActiveServers,
        http: entanglement_core::HttpClient,
    ) -> Self {
        Self {
            avail,
            registry: Arc::downgrade(registry),
            active,
            http,
        }
    }
}

#[async_trait]
impl Tool for McpEnableTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("mcp_enable")
    }

    fn description(&self) -> &str {
        "Enable an available MCP tool server for this session. Its tools are \
         connected on demand and become callable from your next round. Use \
         explore to see which servers are available and their state; \
         enablement is session-scoped and not persisted."
    }

    fn schema(&self) -> serde_json::Value {
        // Static in every mode (#560, ADR-0196 §3/§6): no live server `enum` —
        // that would be its own cache-busting seam. `explore` carries the
        // live roster instead.
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Name of the available MCP server to enable — see explore for the current roster",
                }
            },
            "required": ["server"],
        })
    }

    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        unreachable!("run_for_session is overridden; run/run_content are never called")
    }

    async fn run_for_session(
        &self,
        session: &SessionId,
        _request_id: &str,
        input: &str,
    ) -> anyhow::Result<Vec<ContentPart>> {
        let Input { server } = serde_json::from_str(input)?;
        // The registry outlives every real caller for the process's whole
        // lifetime in practice; `upgrade` only fails during the (never
        // observed outside tests) teardown window after the last strong
        // handle has already dropped.
        let registry = self
            .registry
            .upgrade()
            .context("tool registry is no longer available")?;
        let tools = enable_for_session(
            &self.avail,
            &server,
            session,
            &registry,
            &self.active,
            &self.http,
        )
        .await?;
        Ok(text_parts(format!(
            "enabled {server}, {} tool(s) — list with explore",
            tools.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Mutex, RwLock};

    use super::*;
    use crate::tools::ToolRegistry;

    /// Returns the tool alongside the `SharedRegistry` it holds only a `Weak`
    /// reference to (#556) — the caller must keep this alive for as long as
    /// the tool needs a live registry, exactly like the real owner
    /// (`main.rs`'s `tools` handle) does for the process's lifetime.
    fn tool_with_empty_roster() -> (McpEnableTool, SharedRegistry) {
        let registry: SharedRegistry = Arc::new(RwLock::new(ToolRegistry::new()));
        let tool = McpEnableTool::new(
            Arc::new(AvailableMcp::default()),
            &registry,
            Arc::new(Mutex::new(HashMap::new())),
            entanglement_core::HttpClient::new().unwrap(),
        );
        (tool, registry)
    }

    #[test]
    fn schema_is_static_with_no_live_server_enum() {
        // #560, ADR-0196 §3/§6: the schema never varies by roster — a live
        // `enum` would be its own cache-busting seam. Byte-identical whether
        // the roster is empty or not (a second registry with servers would
        // assert the same schema, so one snapshot suffices as a golden).
        let (tool, _registry) = tool_with_empty_roster();
        let schema = tool.schema();
        assert!(schema["properties"]["server"].get("enum").is_none());
        assert_eq!(schema["properties"]["server"]["type"], "string");
        assert_eq!(schema["required"][0], "server");
    }

    #[tokio::test]
    async fn unknown_server_surfaces_the_error_to_the_model() {
        let (tool, _registry) = tool_with_empty_roster();
        let err = tool
            .run_for_session(&SessionId::new("s"), "r1", r#"{"server":"nope"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no available MCP server"), "{err}");
    }

    #[tokio::test]
    async fn malformed_input_is_an_error_not_a_panic() {
        let (tool, _registry) = tool_with_empty_roster();
        assert!(tool
            .run_for_session(&SessionId::new("s"), "r1", "not json")
            .await
            .is_err());
    }

    /// The whole point of holding `Weak` instead of `Arc` (#556): once every
    /// strong handle on the registry drops, the tool must not keep it alive —
    /// and must fail cleanly, not panic, on its next call.
    #[test]
    fn holds_no_strong_reference_keeping_the_registry_alive() {
        let (tool, registry) = tool_with_empty_roster();
        assert_eq!(
            Arc::strong_count(&registry),
            1,
            "only the test's own handle"
        );
        drop(registry);
        assert!(tool.registry.upgrade().is_none());
    }

    #[tokio::test]
    async fn a_dropped_registry_surfaces_a_clean_error_not_a_panic() {
        let (tool, registry) = tool_with_empty_roster();
        drop(registry);
        let err = tool
            .run_for_session(&SessionId::new("s"), "r1", r#"{"server":"nope"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no longer available"), "{err}");
    }
}
