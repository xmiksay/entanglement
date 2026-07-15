//! The transport-agnostic MCP client (#198, #312).
//!
//! [`McpClient`] is a thin enum over the concrete transports — the stdio
//! subprocess session ([`StdioClient`][super::stdio::StdioClient]) and, behind
//! the `mcp-http` feature, the streamable-HTTP session
//! ([`HttpClient`][super::http::HttpClient]). [`McpTool`][super::tool::McpTool]
//! holds an `Arc<McpClient>` and only ever calls [`list_tools`][McpClient::list_tools]
//! / [`call_tool`][McpClient::call_tool], so it adapts whatever transport backs a
//! server without knowing which one.
//!
//! Embedders that build tools programmatically (per-tenant remote servers with
//! per-user tokens) can construct a transport directly — see
//! [`HttpClient::connect`][super::http::HttpClient::connect] — wrap it in the
//! matching [`McpClient`] variant, and hand it to `McpTool::new`, bypassing the
//! YAML config path entirely.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use super::stdio::StdioClient;
use super::McpServerConfig;

/// A single tool as advertised by a server's `tools/list`.
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A live MCP session over one of the supported transports. Dispatches
/// `tools/list` / `tools/call` to the concrete client.
pub enum McpClient {
    /// JSON-RPC over a spawned subprocess's stdio (#198).
    Stdio(StdioClient),
    /// Streamable HTTP — `POST`ed JSON-RPC with per-server headers (#312).
    #[cfg(feature = "mcp-http")]
    Http(super::http::HttpClient),
}

impl McpClient {
    /// Connect to the server described by `cfg`, resolving its transport from the
    /// `command` XOR `url` fields, and return a shareable handle.
    pub async fn connect(server: &str, cfg: &McpServerConfig) -> Result<Arc<Self>> {
        let client = match cfg.transport()? {
            super::Transport::Stdio { command, args, env } => {
                McpClient::Stdio(StdioClient::spawn(server, &command, &args, &env).await?)
            }
            #[cfg(feature = "mcp-http")]
            super::Transport::Http { url, headers } => {
                McpClient::Http(super::http::HttpClient::connect(server, &url, &headers).await?)
            }
            #[cfg(not(feature = "mcp-http"))]
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

/// Parse one `tools/list` entry, tolerating a missing description or schema. A
/// tool with no `name` is skipped (it can't be called). Shared by every
/// transport.
pub(super) fn parse_tool_def(v: &Value) -> Option<McpToolDef> {
    let name = v.get("name").and_then(Value::as_str)?.to_string();
    let description = v
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input_schema = v
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    Some(McpToolDef {
        name,
        description,
        input_schema,
    })
}

/// Split a parsed JSON-RPC response object into its `result` (`Ok`) or `error`
/// message (`Err`). Shared by every transport's demultiplexer.
pub(super) fn jsonrpc_payload(msg: &Value) -> std::result::Result<Value, String> {
    if let Some(err) = msg.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        Err(message)
    } else {
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }
}
