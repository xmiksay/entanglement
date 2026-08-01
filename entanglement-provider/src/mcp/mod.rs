//! MCP (Model Context Protocol) client mechanism — transport + authentication.
//!
//! Moved here from `entanglement-runtime::mcp` (ADR-0153) so that *mechanism*
//! lives in the leaf crate and *policy* stays in the head: this module knows how
//! to speak the streamable-HTTP transport and how to obtain an OAuth token for
//! it, but nothing about `config.yml`, the `ToolRegistry`, permission profiles,
//! or the three-state activation (#542) — all of which remain runtime-owned.
//! The runtime consumes these types through core's re-export (ADR-0053), the
//! same path [`McpServerState`][crate::provider_mcp::McpServerState] already
//! takes, so the lean `--no-default-features` runtime build reaches them without
//! naming the optional `entanglement-provider` dependency directly.
//!
//! What lives here:
//!
//! - [`http`] — the streamable-HTTP transport ([`McpHttpClient`]), previously
//!   `entanglement-runtime::mcp::http` (#312, ADR-0080).
//! - [`auth`] — the OAuth 2.1 mechanism for authenticating that transport
//!   (ADR-0153): metadata discovery, PKCE, dynamic client registration, and
//!   token exchange/refresh/revocation.
//! - The shared JSON-RPC/tool-definition helpers both the HTTP transport and the
//!   runtime's stdio transport parse responses with.
//!
//! The stdio transport deliberately stays in the runtime: it spawns a
//! subprocess and needs the runtime's provider-key scrub (#164/ADR-0124), which
//! is policy, not mechanism.

use serde_json::{json, Value};

pub mod auth;
pub mod headers;
pub mod http;
pub mod sse;

pub use auth::{
    AccessTokenSource, AuthFlow, AuthOutcome, AuthRequired, ClientRegistration, OauthConfig,
    StoredAuth, TokenSet, TokenStore,
};
pub use http::McpHttpClient;

/// A single tool as advertised by a server's `tools/list`.
///
/// Transport-agnostic: both the HTTP client here and the runtime's stdio client
/// parse into this shape via [`parse_tool_def`].
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Parse one `tools/list` entry, tolerating a missing description or schema. A
/// tool with no `name` is skipped (it can't be called). Shared by every
/// transport.
pub fn parse_tool_def(v: &Value) -> Option<McpToolDef> {
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
pub fn jsonrpc_payload(msg: &Value) -> std::result::Result<Value, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_def_fills_defaults_and_skips_nameless() {
        let full = json!({
            "name": "search",
            "description": "find things",
            "inputSchema": { "type": "object", "properties": { "q": {} } },
        });
        let def = parse_tool_def(&full).unwrap();
        assert_eq!(def.name, "search");
        assert_eq!(def.description, "find things");
        assert_eq!(def.input_schema["properties"]["q"], json!({}));

        // Missing description/schema fall back rather than dropping the tool.
        let bare = json!({ "name": "ping" });
        let def = parse_tool_def(&bare).unwrap();
        assert_eq!(def.description, "");
        assert_eq!(
            def.input_schema,
            json!({ "type": "object", "properties": {} })
        );

        // No name → uncallable → skipped.
        assert!(parse_tool_def(&json!({ "description": "x" })).is_none());
    }

    #[test]
    fn jsonrpc_payload_splits_result_from_error() {
        let ok = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
        assert_eq!(jsonrpc_payload(&ok).unwrap(), json!({ "tools": [] }));

        // A result-less success is `null`, not an error.
        let empty = json!({ "jsonrpc": "2.0", "id": 1 });
        assert_eq!(jsonrpc_payload(&empty).unwrap(), Value::Null);

        let err = json!({ "jsonrpc": "2.0", "id": 1, "error": { "message": "boom" } });
        assert_eq!(jsonrpc_payload(&err).unwrap_err(), "boom");

        // An error object with no `message` still yields something printable.
        let odd = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32000 } });
        assert!(jsonrpc_payload(&odd).unwrap_err().contains("-32000"));
    }
}
