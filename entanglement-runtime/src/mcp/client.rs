//! The transport-agnostic MCP client (#198, #312).
//!
//! [`McpClient`] is a thin enum over the concrete transports — the stdio
//! subprocess session ([`StdioClient`][super::stdio::StdioClient]) and, behind
//! the `mcp-http` feature, the streamable-HTTP session
//! ([`McpHttpClient`]). [`McpTool`][super::tool::McpTool] holds an
//! `Arc<McpClient>` and only ever calls [`list_tools`][McpClient::list_tools] /
//! [`call_tool`][McpClient::call_tool], so it adapts whatever transport backs a
//! server without knowing which one.
//!
//! Since ADR-0153 the HTTP transport, the shared JSON-RPC helpers, and the whole
//! OAuth mechanism live in `entanglement-provider` and are reached through
//! core's re-export (ADR-0053) — the same path `McpServerState` takes, so the
//! lean `--no-default-features` build gets them without naming the optional
//! provider dependency. What stays here is policy: config, the registry, the
//! permission-governed tool wrapper, and the stdio transport's key scrub.
//!
//! Embedders that build tools programmatically (per-tenant remote servers with
//! per-user tokens) can construct a transport directly — see
//! [`McpHttpClient::connect`] — wrap it in the matching [`McpClient`] variant,
//! and hand it to `McpTool::new`, bypassing the YAML config path entirely.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use super::stdio::StdioClient;
use super::McpServerConfig;

// Re-exported so the rest of the runtime (and existing embedders) keep importing
// these from `mcp::client`, unchanged by the ADR-0153 relocation.
pub use entanglement_core::{jsonrpc_payload, parse_tool_def, McpToolDef};
#[cfg(feature = "mcp-http")]
pub use entanglement_core::{AccessTokenSource, McpHttpClient};

/// A live MCP session over one of the supported transports. Dispatches
/// `tools/list` / `tools/call` to the concrete client.
pub enum McpClient {
    /// JSON-RPC over a spawned subprocess's stdio (#198).
    Stdio(StdioClient),
    /// Streamable HTTP — `POST`ed JSON-RPC with per-server headers (#312) and,
    /// optionally, an OAuth bearer token (ADR-0153).
    #[cfg(feature = "mcp-http")]
    Http(McpHttpClient),
}

impl McpClient {
    /// Connect to the server described by `cfg`, resolving its transport from the
    /// `command` XOR `url` fields, and return a shareable handle. `secret_env`
    /// is scrubbed from a stdio server's child environment (#164 parity); the
    /// HTTP transport spawns nothing, so it ignores the list.
    pub async fn connect(
        server: &str,
        cfg: &McpServerConfig,
        secret_env: &[String],
    ) -> Result<Arc<Self>> {
        #[cfg(feature = "mcp-http")]
        {
            Self::connect_with_auth(server, cfg, secret_env, None).await
        }
        #[cfg(not(feature = "mcp-http"))]
        {
            Self::connect_inner(server, cfg, secret_env).await
        }
    }

    /// Connect, supplying an OAuth token source for the HTTP transport
    /// (ADR-0153). `auth` is ignored by the stdio transport, which authenticates
    /// (if at all) through its own environment.
    #[cfg(feature = "mcp-http")]
    pub async fn connect_with_auth(
        server: &str,
        cfg: &McpServerConfig,
        secret_env: &[String],
        auth: Option<Arc<dyn AccessTokenSource>>,
    ) -> Result<Arc<Self>> {
        let client = match cfg.transport()? {
            super::Transport::Stdio { command, args, env } => McpClient::Stdio(
                StdioClient::spawn(server, &command, &args, &env, secret_env).await?,
            ),
            super::Transport::Http { url, headers } => McpClient::Http(match auth {
                Some(auth) => {
                    McpHttpClient::connect_authenticated(server, &url, &headers, auth).await?
                }
                None => McpHttpClient::connect(server, &url, &headers).await?,
            }),
        };
        Ok(Arc::new(client))
    }

    #[cfg(not(feature = "mcp-http"))]
    async fn connect_inner(
        server: &str,
        cfg: &McpServerConfig,
        secret_env: &[String],
    ) -> Result<Arc<Self>> {
        let client = match cfg.transport()? {
            super::Transport::Stdio { command, args, env } => McpClient::Stdio(
                StdioClient::spawn(server, &command, &args, &env, secret_env).await?,
            ),
            super::Transport::Http { .. } => anyhow::bail!(
                "MCP server `{server}` uses the HTTP transport, but this build was compiled \
                 without the `mcp-http` feature"
            ),
        };
        Ok(Arc::new(client))
    }

    /// Discover the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        match self {
            McpClient::Stdio(c) => c.list_tools().await,
            #[cfg(feature = "mcp-http")]
            McpClient::Http(c) => c.list_tools().await,
        }
    }

    /// Invoke one tool by its remote name.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        match self {
            McpClient::Stdio(c) => c.call_tool(name, arguments).await,
            #[cfg(feature = "mcp-http")]
            McpClient::Http(c) => c.call_tool(name, arguments).await,
        }
    }
}
