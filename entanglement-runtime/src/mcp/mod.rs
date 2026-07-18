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
use serde::{Deserialize, Serialize};

use crate::tool_names;
use crate::tools::{Tool, ToolRegistry};

pub mod client;
#[cfg(feature = "mcp-http")]
pub mod http;
pub mod live;
pub mod responder;
pub mod stdio;
pub mod tool;

pub use client::{McpClient, McpToolDef};
#[cfg(feature = "mcp-http")]
pub use http::HttpClient;
pub use live::{mcp_add, mcp_list, mcp_remove, ActiveServer, ActiveServers, ServerConfigs};
pub use responder::spawn_mcp_responder;
pub use stdio::StdioClient;
pub use tool::McpTool;

/// One external MCP server, deserialized straight from the user config's `mcp:`
/// map. The transport is chosen by which of `command` (stdio subprocess) XOR
/// `url` (streamable HTTP) is present; supplying both, or neither, is a config
/// error caught by [`transport`][Self::transport]. `deny_unknown_fields` makes a
/// typo'd key a loud error, matching every other config section. `Serialize`
/// (#375) lets a live `/mcp add`/`remove` write the section straight back to
/// `config.yml` (`crate::config::save_mcp`) — the wire shape stays whatever a
/// hand-edited file already used, field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Config-side capability hint (#426): raw (un-namespaced) tool name → capability
    /// (`read`/`write`/`call`), the same names `tool_names::CAPABILITIES` defines.
    /// An MCP tool's `tools/list` response carries no such hint of its own, so a
    /// profile's bare `read: allow` otherwise never reaches an MCP-provided
    /// read-only tool — it falls through as an ungrouped literal name
    /// (`mcp__<server>__<tool>`). Annotating it here lets
    /// [`capability_index`] fold it into that fan-out. Speculative: a name with
    /// no matching registered tool is simply inert, so this can be set ahead of
    /// (or survive across) the server actually connecting.
    #[serde(default)]
    pub capabilities: HashMap<String, String>,
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

/// Mirrors `entanglement_core::McpServerSpec` (the `InMsg::McpAdd` wire DTO,
/// #375) into the runtime's richer config type. Core carries no MCP logic
/// (ADR-0067), so the `command`/`url` XOR check is deferred to
/// [`McpServerConfig::transport`] — this conversion is a plain field copy. The
/// wire DTO carries no `capabilities` hint (#426, config-only), so a live
/// `/mcp add` always starts with an empty map — annotating capabilities for a
/// live-added server means hand-editing `config.yml` afterward.
impl From<entanglement_core::McpServerSpec> for McpServerConfig {
    fn from(spec: entanglement_core::McpServerSpec) -> Self {
        Self {
            command: spec.command,
            args: spec.args,
            env: spec.env,
            url: spec.url,
            headers: spec.headers,
            disabled: spec.disabled,
            capabilities: HashMap::new(),
        }
    }
}

/// Capability name (`read`/`write`/`call`) → the namespaced MCP tool names
/// (`mcp__<server>__<tool>`) annotated with it across every configured server
/// (#426). This is what lets the shared capability fan-out
/// (`agents::expand_capabilities`) cover MCP tools alongside the fixed
/// built-in set (`tool_names::CAPABILITIES`) — the membership just comes from
/// config instead of a compile-time table.
pub type McpCapabilityIndex = HashMap<String, Vec<String>>;

/// Build the [`McpCapabilityIndex`] from every configured server's
/// `capabilities` annotation (config-only — a live `/mcp add` never sets one,
/// see the `From` impl above). An unknown capability name is a loud config
/// error, matching every other `deny_unknown_fields` section — a typo'd
/// capability should never silently grade as an ungrouped literal tool. Does
/// not require the server to actually be connected: the resulting rules are
/// resolved by tool *name*, so an entry naming a tool the server doesn't
/// (yet, or ever) expose is simply inert.
pub fn capability_index(servers: &HashMap<String, McpServerConfig>) -> Result<McpCapabilityIndex> {
    let mut index: McpCapabilityIndex = HashMap::new();
    for (server, cfg) in servers {
        for (tool, capability) in &cfg.capabilities {
            if !tool_names::is_capability_name(capability) {
                bail!(
                    "MCP server `{server}` capability hint for tool `{tool}`: unknown \
                     capability `{capability}` (expected `read`, `write`, or `call`)"
                );
            }
            index
                .entry(capability.clone())
                .or_default()
                .push(tool::namespaced_tool_name(server, tool));
        }
    }
    for members in index.values_mut() {
        members.sort();
    }
    Ok(index)
}

/// `"stdio"` or `"http"`, for the [`McpServerStatus`][entanglement_core::McpServerStatus]
/// wire label — meaningful only after [`McpServerConfig::transport`] has already
/// validated the XOR, so it just reads back which field was set.
pub(crate) fn transport_label(cfg: &McpServerConfig) -> String {
    if cfg.command.is_some() {
        "stdio"
    } else {
        "http"
    }
    .to_string()
}

/// Connect to every configured server and register its tools into `registry`.
///
/// Best-effort per server: a connect/handshake/`tools/list` failure is logged and
/// skipped so one broken server can't stop startup. The registered [`McpTool`]s
/// hold an `Arc<McpClient>`, so keeping them in `registry` keeps each server
/// connection alive for the process lifetime — no separate handle to retain.
/// Returns the servers that connected, seeding [`ActiveServers`] (#375) so a
/// later live `/mcp list`/`remove` sees exactly what startup actually attached.
pub async fn connect(
    servers: &HashMap<String, McpServerConfig>,
    registry: &mut ToolRegistry,
) -> HashMap<String, ActiveServer> {
    let mut active = HashMap::new();
    for (name, cfg) in servers {
        if cfg.disabled {
            tracing::info!("MCP server `{name}` is disabled; skipping");
            continue;
        }
        match connect_one(name, cfg, registry).await {
            Ok((client, tools)) => {
                active.insert(
                    name.clone(),
                    ActiveServer {
                        client,
                        tools,
                        transport: transport_label(cfg),
                    },
                );
            }
            Err(e) => tracing::warn!("MCP server `{name}`: {e:#}"),
        }
    }
    active
}

/// The two network-I/O awaits (connect + `tools/list`), with no registry
/// involved — split out of the old single-shot `connect_one` (#375) so a live
/// `mcp_add` can run these *before* taking the registry's write lock, never
/// holding it across an `.await`.
async fn connect_client(
    name: &str,
    cfg: &McpServerConfig,
) -> Result<(std::sync::Arc<McpClient>, Vec<McpToolDef>)> {
    let client = McpClient::connect(name, cfg).await?;
    let defs = client.list_tools().await?;
    Ok((client, defs))
}

/// Register every discovered tool def into `registry`, synchronous (no
/// `.await`) so it is safe to run under a held write lock. Returns the
/// registered (already-namespaced) tool names.
fn register_tools(
    registry: &mut ToolRegistry,
    client: &std::sync::Arc<McpClient>,
    name: &str,
    defs: Vec<McpToolDef>,
) -> Vec<String> {
    defs.into_iter()
        .map(|def| {
            let tool = McpTool::new(client.clone(), name, def);
            let tool_name = tool.name().into_owned();
            registry.register(tool);
            tool_name
        })
        .collect()
}

async fn connect_one(
    name: &str,
    cfg: &McpServerConfig,
    registry: &mut ToolRegistry,
) -> Result<(std::sync::Arc<McpClient>, Vec<String>)> {
    let (client, defs) = connect_client(name, cfg).await?;
    let count = defs.len();
    let tools = register_tools(registry, &client, name, defs);
    tracing::info!("MCP server `{name}`: registered {count} tool(s)");
    Ok((client, tools))
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
                capabilities: HashMap::new(),
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
                capabilities: HashMap::new(),
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
                capabilities: HashMap::new(),
            },
        )]);
        let mut registry = ToolRegistry::new();
        connect(&servers, &mut registry).await;
        assert!(registry.is_empty());
    }
}
