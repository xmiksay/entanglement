//! The streamable-HTTP MCP transport (#312, ADR-0080).
//!
//! Remote MCP servers — claude.ai-style integrations, the site's own per-user
//! servers — speak the [streamable-HTTP transport][spec] rather than stdio: the
//! client `POST`s a JSON-RPC message to a single endpoint and the server answers
//! either with a lone `application/json` body or with a `text/event-stream` (SSE)
//! whose events carry the JSON-RPC response. An optional `Mcp-Session-Id` handed
//! back on `initialize` is echoed on every later request. Per-server static
//! headers (e.g. `Authorization: Bearer …`) authenticate the connection; `${VAR}`
//! references in a header value are expanded from the process environment so a
//! token never has to be written into the config file in the clear.
//!
//! [`HttpClient`] mirrors [`StdioClient`][super::stdio::StdioClient]'s surface
//! (`list_tools` / `call_tool`) so [`McpClient`][super::client::McpClient] can
//! dispatch to either. It is public so an embedder can build one directly with a
//! per-user token and register its tools without going through the YAML path.
//!
//! [spec]: https://modelcontextprotocol.io/specification/2025-03-26/basic/transports

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};

use super::client::{jsonrpc_payload, parse_tool_def, McpToolDef};

/// Protocol version we advertise on `initialize`. The server negotiates its own
/// in the response, which we echo back on the `MCP-Protocol-Version` header of
/// every subsequent request (per the 2025-06-18 spec revision).
const PROTOCOL_VERSION: &str = "2025-03-26";

/// The MCP session-id header the server may issue on `initialize` and expects on
/// every later request.
const SESSION_HEADER: &str = "Mcp-Session-Id";
const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";

/// Whole-request ceiling — connect, send, and receive the full response. A hung
/// server surfaces as a tool-failure the model sees rather than parking a turn.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Idle gap on an SSE body: if no chunk arrives within this window the stream is
/// treated as stalled. Bounds a server that opens `text/event-stream` and then
/// never sends the response event.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A live streamable-HTTP session with one MCP server.
pub struct HttpClient {
    /// Server name from config — carried into every error message.
    server: String,
    /// The single JSON-RPC endpoint every request is `POST`ed to.
    url: String,
    http: reqwest::Client,
    /// Static per-server headers (auth etc.), already `${VAR}`-expanded.
    headers: HeaderMap,
    /// The negotiated protocol version, echoed on `MCP-Protocol-Version` after
    /// the handshake. `None` until `initialize` returns.
    protocol_version: Mutex<Option<String>>,
    /// The server-issued session id (`Mcp-Session-Id`), echoed on later requests.
    /// `None` for a stateless server that never issues one.
    session_id: Mutex<Option<String>>,
    next_id: AtomicI64,
}

impl HttpClient {
    /// Build a client against `url` with `headers`, then complete the handshake.
    ///
    /// `headers` values may contain `${VAR}` references, expanded from the
    /// process environment. Exposed publicly so an embedder can assemble a
    /// per-tenant client without the YAML config path.
    pub async fn connect(
        server: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self> {
        let headers =
            build_headers(headers).with_context(|| format!("MCP server `{server}` headers"))?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("building MCP HTTP client")?;
        let client = Self {
            server: server.to_string(),
            url: url.to_string(),
            http,
            headers,
            protocol_version: Mutex::new(None),
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
        };
        client
            .handshake()
            .await
            .with_context(|| format!("MCP server `{server}` handshake"))?;
        Ok(client)
    }

    /// `initialize` then the `notifications/initialized` ack.
    async fn handshake(&self) -> Result<()> {
        let res = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "skutter", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await
            .context("initialize")?;
        // Adopt the server's negotiated version for the `MCP-Protocol-Version`
        // header; fall back to what we advertised.
        let negotiated = res
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(PROTOCOL_VERSION)
            .to_string();
        *self.protocol_version.lock().unwrap() = Some(negotiated);
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// Discover the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let res = self
            .request("tools/list", json!({}))
            .await
            .context("tools/list")?;
        let tools = res
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tools.iter().filter_map(parse_tool_def).collect())
    }

    /// Invoke one tool. Returns the raw `tools/call` result object.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
        .with_context(|| format!("tools/call `{name}`"))
    }

    /// `POST` a JSON-RPC request and await its correlated response, whether the
    /// server answers with a lone JSON body or an SSE stream.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.post(&frame))
            .await
            .map_err(|_| anyhow::anyhow!("MCP server `{}` timed out on `{method}`", self.server))?
            .with_context(|| format!("MCP server `{}` `{method}`", self.server))?;
        self.capture_session_id(response.headers());
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "MCP server `{}` returned HTTP {status}: {body}",
                self.server
            );
        }
        let payload = if is_event_stream(response.headers()) {
            self.read_sse_response(response, id).await?
        } else {
            let body: Value = response
                .json()
                .await
                .context("decoding MCP JSON response")?;
            jsonrpc_payload(&body).map_err(|e| anyhow::anyhow!(e))?
        };
        Ok(payload)
    }

    /// Fire-and-forget notification (no `id`). A well-behaved server answers
    /// `202 Accepted` with no body; anything 2xx is fine.
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.post(&frame))
            .await
            .map_err(|_| anyhow::anyhow!("MCP server `{}` timed out on `{method}`", self.server))?
            .with_context(|| format!("MCP server `{}` `{method}`", self.server))?;
        self.capture_session_id(response.headers());
        if !response.status().is_success() {
            bail!(
                "MCP server `{}` rejected `{method}`: HTTP {}",
                self.server,
                response.status()
            );
        }
        Ok(())
    }

    /// Build and send one `POST`, layering the static headers, the negotiated
    /// protocol version, and the session id (when present) onto the request.
    async fn post(&self, frame: &Value) -> reqwest::Result<reqwest::Response> {
        let mut req = self
            .http
            .post(&self.url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            // Accept both shapes the streamable-HTTP server may answer with.
            .header(ACCEPT, "application/json, text/event-stream")
            .json(frame);
        if let Some(v) = self.protocol_version.lock().unwrap().as_deref() {
            req = req.header(PROTOCOL_HEADER, v);
        }
        if let Some(sid) = self.session_id.lock().unwrap().as_deref() {
            req = req.header(SESSION_HEADER, sid);
        }
        req.send().await
    }

    /// Record the `Mcp-Session-Id` the server issues (typically on `initialize`).
    fn capture_session_id(&self, headers: &HeaderMap) {
        if let Some(sid) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
            *self.session_id.lock().unwrap() = Some(sid.to_string());
        }
    }

    /// Drain an SSE body until the JSON-RPC message answering `id` arrives.
    async fn read_sse_response(&self, response: reqwest::Response, id: i64) -> Result<Value> {
        let mut stream = response.bytes_stream();
        // Buffer raw bytes and split on the `\n` byte: a newline never falls
        // inside a multibyte UTF-8 sequence, so decoding one line at a time can't
        // corrupt a token split across two TCP chunks.
        let mut buf: Vec<u8> = Vec::new();
        let mut data = String::new();
        loop {
            let chunk = match tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(item)) => item.context("reading MCP SSE stream")?,
                Ok(None) => bail!(
                    "MCP server `{}` closed the SSE stream before answering",
                    self.server
                ),
                Err(_) => bail!("MCP server `{}` SSE stream stalled", self.server),
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
}

/// Try to resolve one buffered SSE event's `data` payload against our request
/// `id`. Returns `Some(result)` on a match, `None` for an unrelated message
/// (server notification/request), and an error for a JSON-RPC error response.
fn event_payload(data: &str, id: i64) -> Result<Option<Value>> {
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
fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/event-stream"))
        .unwrap_or(false)
}

/// Parse the config header map into a `HeaderMap`, expanding `${VAR}` references
/// in each value from the environment. An invalid header name/value is a hard
/// error so a misconfigured auth header fails loudly at connect time.
fn build_headers(headers: &HashMap<String, String>) -> Result<HeaderMap> {
    let mut out = HeaderMap::new();
    for (name, raw) in headers {
        let value = expand_env(raw);
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid header name `{name}`"))?;
        let value = HeaderValue::from_str(&value)
            .with_context(|| format!("invalid value for header `{name}`"))?;
        out.insert(name, value);
    }
    Ok(out)
}

/// Expand `${VAR}` references from the process environment. An unset variable
/// expands to an empty string (with a warning) so a missing token yields an
/// obviously-broken auth header rather than a literal `${VAR}` on the wire.
fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated `${` — emit the remainder verbatim.
            out.push_str(&rest[start..]);
            return out;
        };
        let var = &after[..end];
        match std::env::var(var) {
            Ok(v) => out.push_str(&v),
            Err(_) => tracing::warn!("MCP header references unset env var `{var}`"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_present_env_var() {
        std::env::set_var("MCP_TEST_TOKEN_XYZ", "secret");
        assert_eq!(expand_env("Bearer ${MCP_TEST_TOKEN_XYZ}"), "Bearer secret");
        std::env::remove_var("MCP_TEST_TOKEN_XYZ");
    }

    #[test]
    fn unset_env_var_expands_empty() {
        std::env::remove_var("MCP_TEST_MISSING_XYZ");
        assert_eq!(expand_env("Bearer ${MCP_TEST_MISSING_XYZ}"), "Bearer ");
    }

    #[test]
    fn literal_without_vars_is_unchanged() {
        assert_eq!(expand_env("Bearer static-token"), "Bearer static-token");
    }

    #[test]
    fn unterminated_brace_is_verbatim() {
        assert_eq!(expand_env("a${b"), "a${b");
    }

    #[test]
    fn builds_auth_header() {
        let mut h = HashMap::new();
        h.insert("Authorization".to_string(), "Bearer abc".to_string());
        let map = build_headers(&h).unwrap();
        assert_eq!(map.get("authorization").unwrap(), "Bearer abc");
    }

    #[test]
    fn rejects_invalid_header_name() {
        let mut h = HashMap::new();
        h.insert("bad header".to_string(), "x".to_string());
        assert!(build_headers(&h).is_err());
    }

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
