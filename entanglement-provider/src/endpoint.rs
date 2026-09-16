//! Definition-driven HTTP endpoint tools (#560 P8): the provider-owned
//! mechanism a runtime-registered `endpoint__<name>` (or skill-declared
//! `skill__<skill>__<name>`) tool calls through, riding the same per-endpoint
//! pool/retry/rate-limit machinery ([`HttpClient::execute_with_retry`]) as LLM
//! and MCP traffic instead of a bespoke `reqwest` client of its own. Mirrors
//! [`crate::mcp::http::McpHttpClient`]'s json-only surface: no `reqwest` type
//! ever crosses back into the runtime, so `entanglement-runtime` never needs
//! `reqwest` as a direct dependency to build a request through this module.
//!
//! [`EndpointMethod`] parses (and deserializes) case-insensitively, so a
//! malformed `method:` in a runtime config file is a loud "validating merged
//! user config" error at config-load time, exactly like every other
//! `deny_unknown_fields` section — never a lazily-discovered failure the
//! first time the tool is actually called.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};

use crate::client::HttpClient;
use crate::mcp::headers::build_headers;

/// Response body cap (#560 P8): a runaway endpoint response must not blow out
/// the model's context budget the way an unbounded `bash`/`call` output
/// would. 32 KiB, matching the plan's chosen figure.
pub const ENDPOINT_RESPONSE_CAP: usize = 32 * 1024;

/// Marker appended to a truncated body so the model can tell "the real
/// response ended here" apart from "the server's own text happened to stop
/// here".
const TRUNCATION_MARKER: &str = "\n...[truncated: response exceeded 32 KiB]";

/// The HTTP methods a config-declared endpoint may use. A closed set (not a
/// raw `String` carried through to call time) so an unsupported method is a
/// config-load-time error, not a runtime one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
}

impl EndpointMethod {
    /// Parse case-insensitively (`get`/`GET`/`Get` all match). An unknown
    /// method names the offending value so the config error is actionable.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            "HEAD" => Self::Head,
            other => anyhow::bail!(
                "unsupported endpoint HTTP method `{other}` (expected one of \
                 GET/POST/PUT/PATCH/DELETE/HEAD)"
            ),
        })
    }
}

impl<'de> Deserialize<'de> for EndpointMethod {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// One endpoint call's plain outcome — no `reqwest` type crosses this
/// boundary (mirrors [`crate::mcp::http::McpHttpClient`]'s json-only
/// surface).
pub struct EndpointResponse {
    pub status: u16,
    pub body: String,
    pub truncated: bool,
}

/// Call one HTTP endpoint through the shared pool (#559): rate limit, retry,
/// and connection reuse identical to LLM/MCP traffic, keyed by `url` alone
/// (no API key — a config-declared endpoint has no catalog identity to
/// pool-key against). `headers`' values may reference `${VAR}` from the
/// environment, exactly like an MCP server's static headers
/// ([`build_headers`]). Never returns `Err` for a non-2xx status — that's a
/// normal result the caller renders (mirrors how a non-zero `bash` exit is
/// not `is_error`, ADR-0176) — only a transport/config failure (an
/// unreachable host, a malformed header) is an `Err`.
pub async fn call(
    http: &HttpClient,
    method: EndpointMethod,
    url: &str,
    headers: &HashMap<String, String>,
    body: Option<&str>,
) -> Result<EndpointResponse> {
    let header_map = build_headers(headers).context("endpoint headers")?;
    let body_owned = body.map(str::to_string);
    let (response, _guard) = http
        .execute_with_retry(url, None, None, None, "", None, None, || {
            let mut builder = match method {
                EndpointMethod::Get => http.client().get(url),
                EndpointMethod::Post => http.client().post(url),
                EndpointMethod::Put => http.client().put(url),
                EndpointMethod::Patch => http.client().patch(url),
                EndpointMethod::Delete => http.client().delete(url),
                EndpointMethod::Head => http.client().head(url),
            };
            builder = builder.headers(header_map.clone());
            if let Some(b) = body_owned.clone() {
                builder = builder.body(b);
            }
            builder.send()
        })
        .await
        .map_err(|e| anyhow::anyhow!("endpoint `{url}`: {e}"))?;

    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .context("reading endpoint response body")?;
    let (body, truncated) = truncate(text);
    Ok(EndpointResponse {
        status,
        body,
        truncated,
    })
}

/// Cap `text` at [`ENDPOINT_RESPONSE_CAP`] bytes, never splitting a
/// multi-byte UTF-8 char, appending [`TRUNCATION_MARKER`] when it fires.
fn truncate(text: String) -> (String, bool) {
    if text.len() <= ENDPOINT_RESPONSE_CAP {
        return (text, false);
    }
    let mut end = ENDPOINT_RESPONSE_CAP;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str(TRUNCATION_MARKER);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn ensure_shared_state_disabled() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            std::env::set_var("ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE", "1");
        });
    }

    fn test_http_client() -> HttpClient {
        ensure_shared_state_disabled();
        HttpClient::new().unwrap()
    }

    /// Spawn a one-shot raw-socket server answering exactly one connection
    /// with `response` (a full `HTTP/1.1 ...` byte string), returning its
    /// `host:port`. Mirrors `entanglement-provider/src/client/tests.rs`'s
    /// existing mock-server pattern — no axum dev-dependency in this crate.
    fn spawn_one_shot(response: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(response);
                let _ = sock.flush();
            }
        });
        addr.to_string()
    }

    #[test]
    fn method_parse_is_case_insensitive() {
        assert_eq!(EndpointMethod::parse("get").unwrap(), EndpointMethod::Get);
        assert_eq!(EndpointMethod::parse("POST").unwrap(), EndpointMethod::Post);
        assert_eq!(EndpointMethod::parse(" Put ").unwrap(), EndpointMethod::Put);
    }

    #[test]
    fn method_parse_rejects_unknown_verbs() {
        let err = EndpointMethod::parse("FETCH").unwrap_err();
        assert!(format!("{err:#}").contains("FETCH"), "{err:#}");
    }

    #[test]
    fn method_deserializes_from_yaml() {
        let m: EndpointMethod = serde_yaml::from_str("post").unwrap();
        assert_eq!(m, EndpointMethod::Post);
        let err = serde_yaml::from_str::<EndpointMethod>("bogus").unwrap_err();
        assert!(format!("{err}").contains("BOGUS"), "{err}");
    }

    #[test]
    fn short_response_is_not_truncated() {
        let (body, truncated) = truncate("hello".to_string());
        assert_eq!(body, "hello");
        assert!(!truncated);
    }

    #[test]
    fn oversized_response_is_capped_at_a_char_boundary_with_a_marker() {
        // A multi-byte char (3 bytes, 'é' is 2 — use a 3-byte one) straddling
        // the cap must not split; the cap lands mid-character so the
        // boundary search must step back.
        let mut text = "x".repeat(ENDPOINT_RESPONSE_CAP - 1);
        text.push('€'); // 3-byte UTF-8 char straddling the cap boundary
        text.push_str(&"y".repeat(100));
        let (body, truncated) = truncate(text);
        assert!(truncated);
        assert!(body.len() <= ENDPOINT_RESPONSE_CAP + TRUNCATION_MARKER.len());
        assert!(body.ends_with(TRUNCATION_MARKER), "{}", body.len());
        assert!(body.is_char_boundary(body.len() - TRUNCATION_MARKER.len()));
    }

    #[tokio::test]
    async fn call_reaches_a_real_server_and_returns_status_and_body() {
        let addr = spawn_one_shot(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
        );
        let http = test_http_client();
        let resp = call(
            &http,
            EndpointMethod::Get,
            &format!("http://{addr}/"),
            &HashMap::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "hello");
        assert!(!resp.truncated);
    }

    #[tokio::test]
    async fn call_surfaces_a_non_2xx_status_as_a_normal_result_not_an_error() {
        let addr = spawn_one_shot(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n\r\nno such x",
        );
        let http = test_http_client();
        let resp = call(
            &http,
            EndpointMethod::Get,
            &format!("http://{addr}/"),
            &HashMap::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, "no such x");
    }
}
