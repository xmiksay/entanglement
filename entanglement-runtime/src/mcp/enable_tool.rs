//! The `mcp_enable` host tool (#542): the agent's own handle on the
//! `allowed` tier of the three-state MCP model — mirroring `load_skill`'s
//! shape (a real registry tool, profile-graded like any other, no
//! runtime-executor interception).
//!
//! The currently *available* servers ride the tool's JSON schema as a live
//! `enum` (recomputed on every spec snapshot via the dynamic
//! `tool_spec_resolver`, ADR-0096), so a keyless bundle stays invisible to
//! the model — "silently absent" — and a `/key` save mid-session surfaces
//! the newly unlocked names on the next turn with no restart.
//!
//! Enabling connects the server lazily and scopes its tools' visibility to
//! the calling session (`AvailableMcp::mark_enabled` + the runtime's spec
//! filter). Nothing persists — the enablement dies with the session, and
//! `config.yml` is never written (unlike `/mcp add`). Note the newly
//! registered tools reach the model on its *next* round (the spec snapshot
//! is per round); the tool's reply says so.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::{ContentPart, SessionId};
use serde::Deserialize;

use crate::tools::{text_parts, SharedRegistry, Tool};

use super::available::{enable_for_session, AvailableMcp};
use super::live::ActiveServers;

/// See the module docs. Holds a `SharedRegistry` handle that (once this tool
/// is registered) forms an `Arc` cycle with the registry — deliberate: both
/// are process-lifetime singletons, never torn down before exit.
pub struct McpEnableTool {
    avail: Arc<AvailableMcp>,
    registry: SharedRegistry,
    active: ActiveServers,
}

#[derive(Deserialize)]
struct Input {
    server: String,
}

impl McpEnableTool {
    pub fn new(avail: Arc<AvailableMcp>, registry: SharedRegistry, active: ActiveServers) -> Self {
        Self {
            avail,
            registry,
            active,
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
         connected on demand and become callable from your next round. The \
         `server` schema lists the currently available servers; enablement is \
         session-scoped and not persisted."
    }

    fn schema(&self) -> serde_json::Value {
        let names = self.avail.available_names();
        let mut server = serde_json::json!({
            "type": "string",
            "description": "Name of the available MCP server to enable",
        });
        // An empty `enum` is invalid JSON Schema; only constrain when there is
        // something to offer (an empty roster keeps the tool callable but
        // every call will report "no available server").
        if !names.is_empty() {
            server["enum"] = serde_json::json!(names);
        }
        serde_json::json!({
            "type": "object",
            "properties": { "server": server },
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
        let tools =
            enable_for_session(&self.avail, &server, session, &self.registry, &self.active).await?;
        Ok(text_parts(format!(
            "MCP server `{server}` enabled for this session — {} tool(s) available from your \
             next round: {}",
            tools.len(),
            tools.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Mutex, RwLock};

    use super::*;
    use crate::tools::ToolRegistry;

    fn tool_with_empty_roster() -> McpEnableTool {
        McpEnableTool::new(
            Arc::new(AvailableMcp::default()),
            Arc::new(RwLock::new(ToolRegistry::new())),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    #[test]
    fn schema_omits_enum_on_an_empty_roster() {
        let schema = tool_with_empty_roster().schema();
        assert!(schema["properties"]["server"].get("enum").is_none());
        assert_eq!(schema["required"][0], "server");
    }

    #[tokio::test]
    async fn unknown_server_surfaces_the_error_to_the_model() {
        let tool = tool_with_empty_roster();
        let err = tool
            .run_for_session(&SessionId::new("s"), "r1", r#"{"server":"nope"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no available MCP server"), "{err}");
    }

    #[tokio::test]
    async fn malformed_input_is_an_error_not_a_panic() {
        let tool = tool_with_empty_roster();
        assert!(tool
            .run_for_session(&SessionId::new("s"), "r1", "not json")
            .await
            .is_err());
    }
}
