//! The `web_fetch` mechanism (#736, ADR-0220): one SSRF-guarded HTTP GET with
//! manual, re-validated redirects and a streamed byte cap. Provider-owned
//! like every other outbound request (ADR-0053: `reqwest` lives here) — the
//! runtime's `web_fetch` tool parses arguments and renders the result, and
//! never sees a `reqwest` type.
//!
//! The client starts from [`client_builder`] (the crate's one TLS setup) but
//! is its own `reqwest::Client`, not the shared [`crate::HttpClient`]: the
//! SSRF guard has to *be* the DNS resolver, and a resolver is a client-wide
//! setting. It also deliberately skips the endpoint pool — that pool keys
//! resilience state per base URL, persisted cross-process (ADR-0144), and
//! every arbitrary site a model fetches would mint its own entry and state
//! file. A fetch is one attempt; a failure goes back to the model.
//!
//! Never uses a proxy (`no_proxy`): a proxy resolves the name itself, which
//! would bypass [`ssrf::GuardedResolver`] entirely.

mod ssrf;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use reqwest::Url;

use crate::client::client_builder;
pub use ssrf::is_blocked;

/// Default response byte cap.
pub const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Ceiling a caller's `max_bytes` is clamped to.
pub const HARD_MAX_BYTES: usize = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-request budget (headers + body); the whole redirect chain is bounded
/// by [`TOTAL_TIMEOUT`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// What a fetch produced. A non-2xx status is a normal result, not an error
/// (mirrors [`crate::endpoint::call`]) — the page body of a 404 is often
/// exactly what the model needs to see.
#[derive(Debug)]
pub struct FetchOutcome {
    /// The URL that answered, after redirects.
    pub final_url: String,
    pub status: u16,
    /// The response's media type (`text/html`), parameters stripped.
    pub mime: Option<String>,
    pub body: FetchBody,
}

#[derive(Debug)]
pub enum FetchBody {
    /// A textual response, lossily decoded as UTF-8 and cut at the byte cap.
    Text { text: String, truncated: bool },
    /// A non-text response. Its body is never read (no point downloading
    /// megabytes nobody will see) — `size` is the declared `Content-Length`.
    Binary { size: Option<u64> },
}

/// An SSRF-guarded fetch client. Cheap to clone.
#[derive(Clone)]
pub struct WebFetcher {
    client: reqwest::Client,
    guard: ssrf::Guard,
}

impl WebFetcher {
    /// Build the guarded client. Fails only if the TLS backend can't
    /// initialize — a host-environment condition, surfaced as an error.
    pub fn new() -> Result<Self> {
        Self::with_guard(ssrf::Guard::default())
    }

    fn with_guard(guard: ssrf::Guard) -> Result<Self> {
        let resolver = ssrf::GuardedResolver {
            guard: guard.clone(),
        };
        let client = client_builder()
            .dns_resolver(resolver)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the web_fetch client")?;
        Ok(Self { client, guard })
    }

    /// GET `url`, following up to [`MAX_REDIRECTS`] redirects (each target
    /// re-validated), reading at most `max_bytes` (clamped to
    /// [`HARD_MAX_BYTES`]) of a textual body.
    pub async fn fetch(&self, url: &str, max_bytes: usize) -> Result<FetchOutcome> {
        let max_bytes = max_bytes.clamp(1, HARD_MAX_BYTES);
        tokio::time::timeout(TOTAL_TIMEOUT, self.fetch_inner(url, max_bytes))
            .await
            .map_err(|_| anyhow::anyhow!("fetch timed out after {TOTAL_TIMEOUT:?}"))?
    }

    async fn fetch_inner(&self, url: &str, max_bytes: usize) -> Result<FetchOutcome> {
        let mut url = Url::parse(url).with_context(|| format!("invalid URL `{url}`"))?;
        for _ in 0..=MAX_REDIRECTS {
            ssrf::validate_url(&url, &self.guard)?;
            let response = self
                .client
                .get(url.clone())
                .header(
                    USER_AGENT,
                    concat!("entanglement-web-fetch/", env!("CARGO_PKG_VERSION")),
                )
                .header(
                    ACCEPT,
                    "text/markdown, text/html;q=0.9, text/plain;q=0.8, */*;q=0.5",
                )
                .send()
                .await
                // `{:#}`-style chain: reqwest wraps the resolver's refusal, and
                // the refusal is the part the model needs to read.
                .map_err(|e| anyhow::anyhow!("fetching {url}: {}", error_chain(&e)))?;
            if response.status().is_redirection() {
                if let Some(next) = redirect_target(&url, &response)? {
                    url = next;
                    continue;
                }
            }
            return read_body(url, response, max_bytes).await;
        }
        bail!("too many redirects (more than {MAX_REDIRECTS})")
    }
}

fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// The next hop of a 3xx, resolved against the current URL. `None` for a
/// redirect status with no `Location` (a 304, say) — that response is the
/// result.
fn redirect_target(current: &Url, response: &reqwest::Response) -> Result<Option<Url>> {
    let Some(location) = response.headers().get(LOCATION) else {
        return Ok(None);
    };
    let location = location
        .to_str()
        .context("redirect Location header is not valid text")?;
    let next = current
        .join(location)
        .with_context(|| format!("invalid redirect target `{location}`"))?;
    Ok(Some(next))
}

async fn read_body(
    url: Url,
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<FetchOutcome> {
    let status = response.status().as_u16();
    let mime = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|m| !m.is_empty());
    if mime.as_deref().is_some_and(|m| !is_textual(m)) {
        return Ok(FetchOutcome {
            final_url: url.to_string(),
            status,
            mime,
            body: FetchBody::Binary {
                size: response.content_length(),
            },
        });
    }
    let mut bytes = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await.context("reading response body")? {
        let room = max_bytes - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    // No declared type: judge by the bytes. A NUL in the first KiB is the
    // classic binary tell; text never carries one.
    if mime.is_none() && bytes.iter().take(1024).any(|&b| b == 0) {
        return Ok(FetchOutcome {
            final_url: url.to_string(),
            status,
            mime,
            body: FetchBody::Binary {
                size: response.content_length(),
            },
        });
    }
    Ok(FetchOutcome {
        final_url: url.to_string(),
        status,
        mime,
        body: FetchBody::Text {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        },
    })
}

/// Whether a media type is text a model can read. Everything else is
/// reported as a binary placeholder, never inlined (the #727 rule for MCP
/// results, applied here too).
pub fn is_textual(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/ecmascript"
                | "application/x-javascript"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-sh"
        )
}

#[cfg(test)]
mod tests;
