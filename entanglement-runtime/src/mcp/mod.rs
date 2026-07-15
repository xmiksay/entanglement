//! MCP (Model Context Protocol) client — attach external tool servers (#198).
//!
//! An embedding gap the audit flagged: a headless *engine* whose selling point is
//! embedding had no way to pull in tools it doesn't ship. This module closes it as
//! a **runtime-side tool provider**, with no core change (the direction #198
//! prescribes): each MCP server is spawned as a subprocess, its `tools/list` is
//! discovered, and every tool is registered into the same
//! [`ToolRegistry`][crate::tools::ToolRegistry] the host quintet uses. From there
//! the tools flow through the existing seams unchanged — their schemas ride
//! `EngineConfig.tool_specs`, and execution round-trips over `ToolExec` /
//! `ToolResult` like any host tool, governed by the same permission profiles.
//!
//! - [`client`] — the JSON-RPC-over-stdio session ([`McpClient`]).
//! - [`tool`] — the [`McpTool`] proxy that adapts one remote tool to [`Tool`].
//!
//! Servers are declared in the layered user config's `mcp:` section (a map of
//! server name → [`McpServerConfig`]). A server that fails to connect is logged
//! and skipped — an external dependency being down must never stop the engine
//! from starting.

use std::collections::HashMap;

use serde::Deserialize;

use crate::tools::ToolRegistry;

pub mod client;
pub mod tool;

pub use client::McpClient;
pub use tool::McpTool;

/// One external MCP server: the command to spawn and its environment. Deserialized
/// straight from the user config's `mcp:` map. `deny_unknown_fields` makes a typo'd
/// key a loud error, matching every other config section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Executable to spawn (looked up on `PATH`).
    pub command: String,
    /// Arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Skip this server without deleting its block.
    #[serde(default)]
    pub disabled: bool,
}

/// Connect to every configured server and register its tools into `registry`.
///
/// Best-effort per server: a spawn/handshake/`tools/list` failure is logged and
/// skipped so one broken server can't stop startup. The registered [`McpTool`]s
/// hold an `Arc<McpClient>`, so keeping them in `registry` keeps each server
/// subprocess alive for the process lifetime — no separate handle to retain.
pub async fn connect(servers: &HashMap<String, McpServerConfig>, registry: &mut ToolRegistry) {
    for (name, cfg) in servers {
        if cfg.disabled {
            tracing::info!("MCP server `{name}` is disabled; skipping");
            continue;
        }
        if let Err(e) = connect_one(name, cfg, registry).await {
            tracing::warn!("MCP server `{name}`: {e:#}");
        }
    }
}

async fn connect_one(
    name: &str,
    cfg: &McpServerConfig,
    registry: &mut ToolRegistry,
) -> anyhow::Result<()> {
    let client = McpClient::spawn(name, cfg).await?;
    let defs = client.list_tools().await?;
    let count = defs.len();
    for def in defs {
        registry.register(McpTool::new(client.clone(), name, def));
    }
    tracing::info!("MCP server `{name}`: registered {count} tool(s)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_server_block() {
        let yaml = r#"
command: npx
args: ["-y", "@modelcontextprotocol/server-everything"]
env:
  FOO: bar
"#;
        let cfg: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.command, "npx");
        assert_eq!(
            cfg.args,
            vec!["-y", "@modelcontextprotocol/server-everything"]
        );
        assert_eq!(cfg.env.get("FOO").map(String::as_str), Some("bar"));
        assert!(!cfg.disabled);
    }

    #[test]
    fn command_only_block_defaults_the_rest() {
        let cfg: McpServerConfig = serde_yaml::from_str("command: my-server").unwrap();
        assert!(cfg.args.is_empty());
        assert!(cfg.env.is_empty());
        assert!(!cfg.disabled);
    }

    #[test]
    fn unknown_field_is_a_loud_error() {
        assert!(serde_yaml::from_str::<McpServerConfig>("command: x\ntyop: 1").is_err());
    }

    #[tokio::test]
    async fn disabled_server_registers_nothing() {
        let servers = HashMap::from([(
            "off".to_string(),
            McpServerConfig {
                command: "definitely-not-a-real-binary-xyz".to_string(),
                args: vec![],
                env: HashMap::new(),
                disabled: true,
            },
        )]);
        let mut registry = ToolRegistry::new();
        connect(&servers, &mut registry).await;
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn unspawnable_server_is_skipped_not_fatal() {
        let servers = HashMap::from([(
            "broken".to_string(),
            McpServerConfig {
                command: "definitely-not-a-real-binary-xyz".to_string(),
                args: vec![],
                env: HashMap::new(),
                disabled: false,
            },
        )]);
        let mut registry = ToolRegistry::new();
        // Must not panic or hang — the failure is logged and swallowed.
        connect(&servers, &mut registry).await;
        assert!(registry.is_empty());
    }
}
