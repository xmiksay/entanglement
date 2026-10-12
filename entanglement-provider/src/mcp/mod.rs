//! MCP (Model Context Protocol) client mechanism — transport + authentication.
//!
//! Moved here from `entanglement-runtime::mcp` (ADR-0153) so that *mechanism*
//! lives in the leaf crate and *policy* stays in the head: this module knows how
//! to speak the streamable-HTTP transport and how to obtain an OAuth token for
//! it, but nothing about `config.yml`, the `ToolRegistry`, permission profiles,
//! or server availability — all of which remain runtime-owned.
//! The runtime consumes these types through core's re-export (ADR-0053), the
//! same path [`ProviderMcpServer`][crate::provider_mcp::ProviderMcpServer]
//! takes, so the lean `--no-default-features` runtime build reaches them without
//! naming the optional `entanglement-provider` dependency directly.
//!
//! What lives here:
//!
//! - [`http`] — the streamable-HTTP transport ([`McpHttpClient`]), previously
//!   `entanglement-runtime::mcp::http` (#312, ADR-0080). Authenticated via
//!   [`crate::oauth`] — the OAuth 2.1 mechanism started life here as
//!   `mcp::auth` (ADR-0153) and was promoted to a crate-level module when the
//!   LLM wires became its second consumer (ADR-0189).
//! - The shared JSON-RPC/tool-definition helpers both the HTTP transport and the
//!   runtime's stdio transport parse responses with — including `tools/list`
//!   pagination ([`list_tools_paginated`]) and the reply to a server-initiated
//!   request such as `ping` ([`server_request_reply`]) (#727).
//!
//! The stdio transport deliberately stays in the runtime: it spawns a
//! subprocess and needs the runtime's provider-key scrub (#164/ADR-0124), which
//! is policy, not mechanism.

use std::future::Future;

use anyhow::{bail, Context};
use serde_json::{json, Value};

pub mod headers;
pub mod http;
pub mod sse;

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

/// Ceiling on `tools/list` pages followed for one discovery (#727). A server
/// whose `nextCursor` never runs out (a cursor bug that loops back to page one)
/// must fail discovery loudly instead of hanging the connect or growing the
/// tool list without bound.
pub const MAX_TOOLS_LIST_PAGES: usize = 100;

/// Run a full `tools/list` discovery, following `nextCursor` until the server
/// stops returning one (#727) — a paginating server otherwise silently loses
/// every tool past its first page. `fetch` sends one `tools/list` request with
/// the given params and returns its `result`; shared by every transport.
pub async fn list_tools_paginated<F, Fut>(mut fetch: F) -> anyhow::Result<Vec<McpToolDef>>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = anyhow::Result<Value>>,
{
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 1..=MAX_TOOLS_LIST_PAGES {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let res = fetch(params)
            .await
            .with_context(|| format!("tools/list page {page}"))?;
        if let Some(list) = res.get("tools").and_then(Value::as_array) {
            tools.extend(list.iter().filter_map(parse_tool_def));
        }
        match res.get("nextCursor").and_then(Value::as_str) {
            Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
            _ => return Ok(tools),
        }
    }
    bail!(
        "tools/list still returned a nextCursor after {MAX_TOOLS_LIST_PAGES} pages — the \
         server's pagination appears to loop"
    )
}

/// The JSON-RPC reply to a server-initiated *request* (`method` + non-null
/// `id`), or `None` for anything else (a response, a notification). `ping`
/// gets the empty result the spec requires; any other method gets
/// `-32601 Method not found` — this client advertises no capability that
/// would invite one, and an unanswered request could leave the server waiting.
pub fn server_request_reply(msg: &Value) -> Option<Value> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id").filter(|id| !id.is_null())?;
    Some(if method == "ping" {
        json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method `{method}` not supported") },
        })
    })
}

/// Whether `msg` answers one of *our* requests: an `id` but no `method`. Each
/// side numbers its requests independently, so a server's own `ping` may
/// reuse an id we have in flight — matching on `id` alone would resolve our
/// pending call with the server's request.
pub fn is_response(msg: &Value) -> bool {
    msg.get("method").is_none() && msg.get("id").is_some()
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

    /// A scripted `tools/list` fetch: page `n` answers with tool `t<n>` and a
    /// `nextCursor` until page `pages`; `None` never stops paginating.
    async fn paginate(pages: Option<usize>) -> anyhow::Result<(Vec<McpToolDef>, Vec<Value>)> {
        let mut seen = Vec::new();
        let tools =
            list_tools_paginated(|params| {
                seen.push(params);
                let n = seen.len();
                let next = match pages {
                    Some(p) if n >= p => Value::Null,
                    _ => json!(format!("c{n}")),
                };
                async move {
                    Ok(json!({ "tools": [ { "name": format!("t{n}") } ], "nextCursor": next }))
                }
            })
            .await?;
        Ok((tools, seen))
    }

    #[tokio::test]
    async fn pagination_follows_next_cursor_until_absent() {
        let (tools, seen) = paginate(Some(3)).await.unwrap();
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["t1", "t2", "t3"]);
        // The first page carries no cursor; each later one echoes the last.
        assert_eq!(
            seen,
            [
                json!({}),
                json!({ "cursor": "c1" }),
                json!({ "cursor": "c2" })
            ]
        );
    }

    #[tokio::test]
    async fn pagination_errors_on_a_looping_server() {
        let err = paginate(None).await.unwrap_err();
        assert!(format!("{err:#}").contains("100 pages"), "{err:#}");
    }

    #[tokio::test]
    async fn pagination_treats_an_empty_cursor_as_the_last_page() {
        let tools = list_tools_paginated(|_| async {
            Ok(json!({ "tools": [ { "name": "only" } ], "nextCursor": "" }))
        })
        .await
        .unwrap();
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn ping_gets_an_empty_result_and_other_requests_method_not_found() {
        let ping = json!({ "jsonrpc": "2.0", "id": "p1", "method": "ping" });
        assert_eq!(
            server_request_reply(&ping).unwrap(),
            json!({ "jsonrpc": "2.0", "id": "p1", "result": {} })
        );
        let other = json!({ "jsonrpc": "2.0", "id": 7, "method": "sampling/createMessage" });
        assert_eq!(
            server_request_reply(&other).unwrap()["error"]["code"],
            -32601
        );
        // Notifications and responses need no reply.
        let notif = json!({ "jsonrpc": "2.0", "method": "notifications/message" });
        assert!(server_request_reply(&notif).is_none());
        let resp = json!({ "jsonrpc": "2.0", "id": 1, "result": {} });
        assert!(server_request_reply(&resp).is_none());
    }

    #[test]
    fn a_server_request_reusing_our_id_is_not_a_response() {
        assert!(is_response(&json!({ "id": 1, "result": {} })));
        assert!(!is_response(&json!({ "id": 1, "method": "ping" })));
        assert!(!is_response(&json!({ "method": "notifications/x" })));
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
