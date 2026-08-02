//! The streamable-HTTP MCP transport (#312, ADR-0080; moved here by ADR-0153).
//!
//! Remote MCP servers — claude.ai-style integrations, a site's own per-user
//! servers — speak the [streamable-HTTP transport][spec] rather than stdio: the
//! client `POST`s a JSON-RPC message to a single endpoint and the server answers
//! either with a lone `application/json` body or with a `text/event-stream` (SSE)
//! whose events carry the JSON-RPC response. An optional `Mcp-Session-Id` handed
//! back on `initialize` is echoed on every later request.
//!
//! Two ways to authenticate:
//!
//! - **Static headers** — `Authorization: Bearer …` set in config, `${VAR}`-expanded
//!   from the environment (see [`headers`][super::headers]). Right for a
//!   long-lived token the user pastes in.
//! - **OAuth** — an [`AccessTokenSource`][super::auth::AccessTokenSource] supplies a
//!   bearer token per request and refreshes it on demand (ADR-0153). A `401` is
//!   retried exactly once against a force-refreshed token; if it still fails, the
//!   error carries an [`AuthRequired`] the runtime downcasts to report "needs
//!   auth" rather than a generic transport failure.
//!
//! [`McpHttpClient`] mirrors the runtime's stdio client surface (`list_tools` /
//! `call_tool`) so the runtime's `McpClient` enum can dispatch to either. It is
//! public so an embedder can build one directly with a per-user token and
//! register its tools without going through the YAML config path.
//!
//! Every request rides the shared [`HttpClient`] endpoint pool (#559) — the
//! same connection pool, RPM/concurrency caps, and 429/`Retry-After` handling
//! LLM traffic gets — instead of an unmetered bare `reqwest::Client`. This
//! matters for provider-bundled servers (z.ai's `web_search_prime`/
//! `web_reader`/`zread`) that share their provider's API key: without this,
//! a search-heavy turn's MCP calls silently bypassed the same key's LLM
//! endpoint budget. `connect`/`connect_authenticated` take the caller's
//! `HttpClient` and an optional `api_key` for pool-key identity; the server
//! still gets its own [`EndpointState`](crate::client) bucket (keyed by its
//! own URL), isolated from the LLM endpoint sharing its key.
//!
//! [spec]: https://modelcontextprotocol.io/specification/2025-03-26/basic/transports

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::client::{HttpClient, StreamGuard};

use super::auth::{AccessTokenSource, AuthRequired};
use super::headers::build_headers;
use super::sse::{is_event_stream, read_sse_response};
use super::{jsonrpc_payload, parse_tool_def, McpToolDef};

/// Protocol version we advertise on `initialize`. The server negotiates its own
/// in the response, which we echo back on the `MCP-Protocol-Version` header of
/// every subsequent request (per the 2025-06-18 spec revision).
const PROTOCOL_VERSION: &str = "2025-03-26";

/// The MCP session-id header the server may issue on `initialize` and expects on
/// every later request.
const SESSION_HEADER: &str = "Mcp-Session-Id";
const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";

/// The challenge header a `401` carries, pointing at the protected-resource
/// metadata document (RFC 9728) that bootstraps OAuth discovery.
const WWW_AUTHENTICATE: &str = "WWW-Authenticate";

/// A live streamable-HTTP session with one MCP server.
pub struct McpHttpClient {
    /// Server name from config — carried into every error message.
    server: String,
    /// The single JSON-RPC endpoint every request is `POST`ed to.
    url: String,
    /// The shared per-endpoint pool (#559): MCP traffic rides the same
    /// connection pool, RPM/concurrency caps, and 429/`Retry-After` handling
    /// as LLM traffic, instead of an unmetered bare `reqwest::Client`.
    http: HttpClient,
    /// The provider API key this server shares with its LLM endpoint (e.g.
    /// z.ai's bundled `web_search_prime`/`web_reader`/`zread`, all billed
    /// against `ZAI_API_KEY`), when known (#559) — feeds the pool key
    /// alongside `url`, so this server still gets its *own* endpoint bucket
    /// (keyed by its own URL, distinct from the LLM endpoint's — the MCP path
    /// is plausibly a separate rate-limit domain server-side) while sharing
    /// key-hash conventions with the LLM client. `None` for a user-declared
    /// or OAuth-authenticated server, which gets an unkeyed (still pooled)
    /// bucket.
    api_key: Option<String>,
    /// Static per-server headers (auth etc.), already `${VAR}`-expanded.
    headers: HeaderMap,
    /// OAuth bearer-token source (ADR-0153). `None` for a server authenticated
    /// by static headers alone, or by nothing at all.
    auth: Option<Arc<dyn AccessTokenSource>>,
    /// The negotiated protocol version, echoed on `MCP-Protocol-Version` after
    /// the handshake. `None` until `initialize` returns.
    protocol_version: Mutex<Option<String>>,
    /// The server-issued session id (`Mcp-Session-Id`), echoed on later requests.
    /// `None` for a stateless server that never issues one.
    session_id: Mutex<Option<String>>,
    next_id: AtomicI64,
}

impl McpHttpClient {
    /// Build a client against `url` with static `headers`, then complete the
    /// handshake. `http` is the shared per-endpoint pool (#559) every request
    /// rides; `api_key` is the provider key this server shares with its LLM
    /// endpoint, if any (bundled servers), for pool-key identity — `None` for
    /// a user-declared server, which still pools/rate-limits, just under an
    /// unkeyed bucket.
    pub async fn connect(
        server: &str,
        url: &str,
        headers: &HashMap<String, String>,
        http: HttpClient,
        api_key: Option<String>,
    ) -> Result<Self> {
        Self::connect_with(server, url, headers, None, http, api_key).await
    }

    /// Build a client whose `Authorization` header comes from an OAuth token
    /// source (ADR-0153) rather than (or in addition to) static headers. The
    /// source is consulted per request and force-refreshed once on a `401`.
    /// See [`connect`][Self::connect] for `http`/`api_key`.
    pub async fn connect_authenticated(
        server: &str,
        url: &str,
        headers: &HashMap<String, String>,
        auth: Arc<dyn AccessTokenSource>,
        http: HttpClient,
        api_key: Option<String>,
    ) -> Result<Self> {
        Self::connect_with(server, url, headers, Some(auth), http, api_key).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn connect_with(
        server: &str,
        url: &str,
        headers: &HashMap<String, String>,
        auth: Option<Arc<dyn AccessTokenSource>>,
        http: HttpClient,
        api_key: Option<String>,
    ) -> Result<Self> {
        let headers =
            build_headers(headers).with_context(|| format!("MCP server `{server}` headers"))?;
        let client = Self {
            server: server.to_string(),
            url: url.to_string(),
            http,
            api_key,
            headers,
            auth,
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
    /// server answers with a lone JSON body or an SSE stream. The endpoint's
    /// concurrency permit (`_guard`) is held for this whole function — through
    /// the SSE drain or the JSON body read — so an in-flight MCP call counts
    /// against the endpoint's cap for its full duration, mirroring the LLM
    /// clients' `spawn_byte_stream` (#559).
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (response, _guard) = self.send(&frame, method).await?;
        self.capture_session_id(response.headers());
        if !response.status().is_success() {
            return Err(self.status_error(response, method).await);
        }
        let payload = if is_event_stream(response.headers()) {
            read_sse_response(&self.server, response, id).await?
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
        let (response, _guard) = self.send(&frame, method).await?;
        self.capture_session_id(response.headers());
        if !response.status().is_success() {
            return Err(self.status_error(response, method).await);
        }
        Ok(())
    }

    /// Send one request through the shared endpoint pool, retrying exactly once
    /// against a force-refreshed token when the server answers `401` and an
    /// OAuth source is wired. One retry only: a second `401` means the refresh
    /// itself is not enough (revoked grant, changed scopes) and the user has to
    /// re-authorize. Connect/retry/backoff and 429/`Retry-After` pacing are
    /// `HttpClient::execute_with_retry`'s job now (#559) — the old flat 60s
    /// whole-request timeout is gone, the same tradeoff the LLM clients
    /// already make: a 429 is patiently retried rather than treated as a hang.
    async fn send(&self, frame: &Value, method: &str) -> Result<(reqwest::Response, StreamGuard)> {
        let (response, guard) = self.post_once(frame, method, false).await?;
        if response.status() == StatusCode::UNAUTHORIZED && self.auth.is_some() {
            return self.post_once(frame, method, true).await;
        }
        Ok((response, guard))
    }

    /// Build and send one `POST` through the shared per-endpoint pool (#559),
    /// layering the static headers, the OAuth bearer (when configured), the
    /// negotiated protocol version, and the session id. The pool key is
    /// `(self.url, self.api_key)` — this server's own bucket, isolated from
    /// its provider's LLM endpoint, but sharing key-hash identity with it.
    async fn post_once(
        &self,
        frame: &Value,
        method: &str,
        force_refresh: bool,
    ) -> Result<(reqwest::Response, StreamGuard)> {
        let mut headers = self.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Accept both shapes the streamable-HTTP server may answer with.
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(auth) = &self.auth {
            // A failure here (no stored token, refresh rejected) is itself an
            // authorization problem, not a transport one — surface it as such so
            // the runtime reports "needs auth".
            let token = auth.access_token(force_refresh).await.map_err(|e| {
                anyhow::Error::new(AuthRequired {
                    server: self.server.clone(),
                    resource_metadata: None,
                })
                .context(format!("obtaining an access token: {e}"))
            })?;
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("building the MCP bearer header")?;
            headers.insert(AUTHORIZATION, value);
        }
        if let Some(v) = self.protocol_version.lock().unwrap().clone() {
            let value =
                HeaderValue::from_str(&v).context("building the MCP-Protocol-Version header")?;
            headers.insert(PROTOCOL_HEADER, value);
        }
        if let Some(sid) = self.session_id.lock().unwrap().clone() {
            let value =
                HeaderValue::from_str(&sid).context("building the Mcp-Session-Id header")?;
            headers.insert(SESSION_HEADER, value);
        }
        self.http
            .execute_with_retry(
                &self.url,
                self.api_key.as_deref(),
                None,
                None,
                "",
                None,
                || {
                    self.http
                        .client()
                        .post(&self.url)
                        .headers(headers.clone())
                        .json(frame)
                        .send()
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("MCP server `{}` `{method}`: {e}", self.server))
    }

    /// Turn a non-2xx response into an error, tagging a `401` with
    /// [`AuthRequired`] (carrying the RFC 9728 `resource_metadata` pointer from
    /// the `WWW-Authenticate` challenge when present) so the caller can tell
    /// "needs authorization" apart from a generic server failure.
    async fn status_error(&self, response: reqwest::Response, method: &str) -> anyhow::Error {
        let status = response.status();
        let resource_metadata = resource_metadata_url(response.headers());
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            return anyhow::Error::new(AuthRequired {
                server: self.server.clone(),
                resource_metadata,
            })
            .context(format!("`{method}` returned HTTP {status}"));
        }
        anyhow::anyhow!(
            "MCP server `{}` returned HTTP {status} on `{method}`: {body}",
            self.server
        )
    }

    /// Record the `Mcp-Session-Id` the server issues (typically on `initialize`).
    fn capture_session_id(&self, headers: &HeaderMap) {
        if let Some(sid) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
            *self.session_id.lock().unwrap() = Some(sid.to_string());
        }
    }
}

/// Extract the `resource_metadata="…"` parameter from a `WWW-Authenticate`
/// challenge (RFC 9728 §5.1). Returns `None` when the header is absent or
/// carries no such parameter — discovery then falls back to the well-known path
/// derived from the server URL.
pub fn resource_metadata_url(headers: &HeaderMap) -> Option<String> {
    let challenge = headers.get(WWW_AUTHENTICATE)?.to_str().ok()?;
    let start = challenge.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &challenge[start..];
    let value = if let Some(unquoted) = rest.strip_prefix('"') {
        unquoted.split('"').next()?
    } else {
        // Unquoted parameters run to the next comma or whitespace.
        rest.split([',', ' ']).next()?
    };
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn challenge(v: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(WWW_AUTHENTICATE, HeaderValue::from_static(v));
        h
    }

    #[test]
    fn extracts_quoted_resource_metadata() {
        let h = challenge(
            r#"Bearer error="invalid_token", resource_metadata="https://ex.com/.well-known/oauth-protected-resource""#,
        );
        assert_eq!(
            resource_metadata_url(&h).unwrap(),
            "https://ex.com/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn extracts_unquoted_resource_metadata() {
        let h = challenge("Bearer resource_metadata=https://ex.com/meta, error=x");
        assert_eq!(resource_metadata_url(&h).unwrap(), "https://ex.com/meta");
    }

    #[test]
    fn absent_or_irrelevant_challenge_yields_none() {
        assert!(resource_metadata_url(&HeaderMap::new()).is_none());
        assert!(resource_metadata_url(&challenge("Bearer error=\"invalid_token\"")).is_none());
    }
}
