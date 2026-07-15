//! MCP (Model Context Protocol) client — attach external tool servers (#198, #312).
//!
//! An embedding gap the audit flagged: a headless *engine* whose selling point is
//! embedding had no way to pull in tools it doesn't ship. This module closes it as
//! a **runtime-side tool provider**, with no core change (the direction #198
//! prescribes): each MCP server's `tools/list` is discovered and every tool is
//! registered into the same [`ToolRegistry`][crate::tools::ToolRegistry] the host
//! quintet uses. From there the tools flow through the existing seams unchanged —
//! their schemas ride `EngineConfig.tool_specs`, and execution round-trips over
//! `ToolExec` / `ToolResult` like any host tool, governed by the same permission
//! profiles.
//!
//! Two transports back a server, chosen by config (`command` XOR `url`):
//!
//! - [`stdio`] — a spawned subprocess speaking JSON-RPC over its stdio (#198).
//! - [`http`] — a remote server over the streamable-HTTP transport with per-server
//!   auth headers (#312), behind the `mcp-http` feature.
//!
//! [`client`] wraps both behind [`McpClient`]; [`tool`]'s [`McpTool`] proxy adapts
//! whichever transport backs it.
//!
//! Servers are declared in the layered user config's `mcp:` section (a map of
//! server name → [`McpServerConfig`]). A server that fails to connect is logged
//! and skipped — an external dependency being down must never stop the engine
//! from starting.

use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::tools::ToolRegistry;

pub mod client;
#[cfg(feature = "mcp-http")]
pub mod http;
pub mod stdio;
pub mod tool;

pub use client::{McpClient, McpToolDef};
#[cfg(feature = "mcp-http")]
pub use http::HttpClient;
pub use stdio::StdioClient;
pub use tool::McpTool;

/// One external MCP server, deserialized straight from the user config's `mcp:`
/// map. The transport is chosen by which of `command` (stdio subprocess) XOR
/// `url` (streamable HTTP) is present; supplying both, or neither, is a config
/// error caught by [`transport`][Self::transport]. `deny_unknown_fields` makes a
/// typo'd key a loud error, matching every other config section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// stdio transport: executable to spawn (looked up on `PATH`).
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments passed to the command (stdio transport only).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the server process (stdio transport only).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP transport: the streamable-HTTP endpoint (e.g. `https://example.com/mcp`).
    #[serde(default)]
    pub url: Option<String>,
    /// Static request headers for the HTTP transport (e.g. `Authorization`).
    /// Values may reference `${VAR}` from the environment.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Skip this server without deleting its block.
    #[serde(default)]
    pub disabled: bool,
}

/// The resolved transport for one server — the `command` XOR `url` choice made
/// concrete, carrying only the fields that transport uses.
pub enum Transport {
    /// Spawn a subprocess and speak JSON-RPC over its stdio.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// `POST` JSON-RPC to a remote streamable-HTTP endpoint.
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
}

impl McpServerConfig {
    /// Resolve the transport, enforcing the `command` XOR `url` rule.
    pub fn transport(&self) -> Result<Transport> {
        match (self.command.as_deref(), self.url.as_deref()) {
            (Some(command), None) => Ok(Transport::Stdio {
                command: command.to_string(),
                args: self.args.clone(),
                env: self.env.clone(),
            }),
            (None, Some(url)) => Ok(Transport::Http {
                url: url.to_string(),
                headers: self.headers.clone(),
            }),
            (Some(_), Some(_)) => {
                bail!("MCP server sets both `command` and `url` — pick one transport")
            }
            (None, None) => bail!("MCP server sets neither `command` nor `url`"),
        }
    }
}

/// Connect to every configured server and register its tools into `registry`.
///
/// Best-effort per server: a connect/handshake/`tools/list` failure is logged and
/// skipped so one broken server can't stop startup. The registered [`McpTool`]s
/// hold an `Arc<McpClient>`, so keeping them in `registry` keeps each server
/// connection alive for the process lifetime — no separate handle to retain.
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

async fn connect_one(name: &str, cfg: &McpServerConfig, registry: &mut ToolRegistry) -> Result<()> {
    let client = McpClient::connect(name, cfg).await?;
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
    fn parses_a_stdio_block() {
        let yaml = r#"
command: npx
args: ["-y", "@modelcontextprotocol/server-everything"]
env:
  FOO: bar
"#;
        let cfg: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.command.as_deref(), Some("npx"));
        assert_eq!(
            cfg.args,
            vec!["-y", "@modelcontextprotocol/server-everything"]
        );
        assert_eq!(cfg.env.get("FOO").map(String::as_str), Some("bar"));
        assert!(!cfg.disabled);
        assert!(matches!(cfg.transport().unwrap(), Transport::Stdio { .. }));
    }

    #[test]
    fn parses_an_http_block() {
        let yaml = r#"
url: https://example.com/mcp
headers:
  Authorization: "Bearer xyz"
"#;
        let cfg: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.url.as_deref(), Some("https://example.com/mcp"));
        match cfg.transport().unwrap() {
            Transport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer xyz")
                );
            }
            _ => panic!("expected HTTP transport"),
        }
    }

    #[test]
    fn command_only_block_defaults_the_rest() {
        let cfg: McpServerConfig = serde_yaml::from_str("command: my-server").unwrap();
        assert!(cfg.args.is_empty());
        assert!(cfg.env.is_empty());
        assert!(!cfg.disabled);
    }

    #[test]
    fn both_transports_is_an_error() {
        let cfg: McpServerConfig =
            serde_yaml::from_str("command: x\nurl: https://example.com/mcp").unwrap();
        assert!(cfg.transport().is_err());
    }

    #[test]
    fn neither_transport_is_an_error() {
        let cfg: McpServerConfig = serde_yaml::from_str("disabled: false").unwrap();
        assert!(cfg.transport().is_err());
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
                command: Some("definitely-not-a-real-binary-xyz".to_string()),
                args: vec![],
                env: HashMap::new(),
                url: None,
                headers: HashMap::new(),
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
                command: Some("definitely-not-a-real-binary-xyz".to_string()),
                args: vec![],
                env: HashMap::new(),
                url: None,
                headers: HashMap::new(),
                disabled: false,
            },
        )]);
        let mut registry = ToolRegistry::new();
        // Must not panic or hang — the failure is logged and swallowed.
        connect(&servers, &mut registry).await;
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn unreachable_http_server_is_skipped_not_fatal() {
        let servers = HashMap::from([(
            "remote".to_string(),
            McpServerConfig {
                command: None,
                args: vec![],
                env: HashMap::new(),
                // Reserved TEST-NET-1 address that never answers → connect fails fast.
                url: Some("http://192.0.2.1:1/mcp".to_string()),
                headers: HashMap::new(),
                disabled: false,
            },
        )]);
        let mut registry = ToolRegistry::new();
        connect(&servers, &mut registry).await;
        assert!(registry.is_empty());
    }
}
