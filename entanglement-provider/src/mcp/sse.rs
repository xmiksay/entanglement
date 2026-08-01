//! SSE response demultiplexing for the MCP streamable-HTTP transport (#312).
//!
//! Split out of `http.rs` along the 400-line file cap when the transport moved
//! into this crate (ADR-0153). A streamable-HTTP server may answer a `POST`
//! either with a lone `application/json` body or with a `text/event-stream`
//! whose events carry the JSON-RPC response — plus, on the same stream,
//! unrelated server notifications. These helpers drain the stream until the
//! event answering *our* request id arrives.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_TYPE};
use serde_json::Value;

use super::jsonrpc_payload;

/// Idle gap on an SSE body: if no chunk arrives within this window the stream is
/// treated as stalled. Bounds a server that opens `text/event-stream` and then
/// never sends the response event.
pub const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Drain an SSE body until the JSON-RPC message answering `id` arrives.
/// `server` names the server in error messages only.
pub async fn read_sse_response(
    server: &str,
    response: reqwest::Response,
    id: i64,
) -> Result<Value> {
    let mut stream = response.bytes_stream();
    // Buffer raw bytes and split on the `\n` byte: a newline never falls
    // inside a multibyte UTF-8 sequence, so decoding one line at a time can't
    // corrupt a token split across two TCP chunks.
    let mut buf: Vec<u8> = Vec::new();
    let mut data = String::new();
    loop {
        let chunk = match tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(item)) => item.context("reading MCP SSE stream")?,
            Ok(None) => bail!("MCP server `{server}` closed the SSE stream before answering"),
            Err(_) => bail!("MCP server `{server}` SSE stream stalled"),
        };
        buf.extend_from_slice(&chunk);
        // An SSE event ends at a blank line; a `data:` field may span lines.
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw[..nl]);
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                // End of one event: try to resolve it, else reset and continue.
                if let Some(v) = event_payload(&data, id)? {
                    return Ok(v);
                }
                data.clear();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
            // Other SSE fields (`event:`, `id:`, comments) are ignored.
        }
    }
}

/// Try to resolve one buffered SSE event's `data` payload against our request
/// `id`. Returns `Some(result)` on a match, `None` for an unrelated message
/// (server notification/request), and an error for a JSON-RPC error response.
pub fn event_payload(data: &str, id: i64) -> Result<Option<Value>> {
    if data.is_empty() {
        return Ok(None);
    }
    let msg: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        // A non-JSON event is not our response — skip it rather than abort.
        Err(_) => return Ok(None),
    };
    if msg.get("id").and_then(Value::as_i64) != Some(id) {
        return Ok(None);
    }
    jsonrpc_payload(&msg)
        .map(Some)
        .map_err(|e| anyhow::anyhow!(e))
}

/// Does the response advertise an SSE body?
pub fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/event-stream"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    #[test]
    fn event_payload_matches_id_and_skips_others() {
        // Unrelated notification → skipped.
        let notif = r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#;
        assert!(event_payload(notif, 1).unwrap().is_none());
        // Wrong id → skipped.
        let other = r#"{"jsonrpc":"2.0","id":2,"result":{"ok":true}}"#;
        assert!(event_payload(other, 1).unwrap().is_none());
        // Matching id → resolved.
        let ours = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        assert_eq!(
            event_payload(ours, 1).unwrap().unwrap(),
            json!({ "tools": [] })
        );
        // Matching id but a JSON-RPC error → error.
        let err = r#"{"jsonrpc":"2.0","id":1,"error":{"message":"boom"}}"#;
        assert!(event_payload(err, 1).is_err());
    }

    #[test]
    fn non_json_event_is_skipped_not_fatal() {
        assert!(event_payload("ping", 1).unwrap().is_none());
        assert!(event_payload("", 1).unwrap().is_none());
    }

    #[test]
    fn detects_event_stream_content_type() {
        let mut h = HeaderMap::new();
        h.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_event_stream(&h));
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(!is_event_stream(&h));
    }
}
