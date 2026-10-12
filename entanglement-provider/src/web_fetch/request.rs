//! The general request behind both `web_fetch` (a GET) and the rhai `http()`
//! binding (ADR-0227): any method, caller headers and body, the same SSRF
//! guard and manual re-validated redirects. Two caller seams run per hop:
//! `check_hop` grades the next `(method, url)` before it is requested, and
//! `credentials` supplies the hidden headers for the URL about to be sent —
//! asked again on every hop, so the caller decides per target (the runtime
//! drops them on a cross-host or non-https hop).

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderName, HeaderValue, USER_AGENT};
use reqwest::{Method, StatusCode, Url};

use super::{error_chain, HARD_MAX_BYTES, MAX_REDIRECTS, TOTAL_TIMEOUT};
use super::{parse_normalized, read_body, redirect_target, ssrf, FetchOutcome, WebFetcher};

/// The methods `http()` accepts — a closed set, so a typo fails before any
/// grading or network.
pub const HTTP_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// One request. `headers` are the caller's own (never credentials — those
/// come from the per-hop seam).
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub max_bytes: usize,
}

/// Uppercase `raw` and check it against [`HTTP_METHODS`].
pub fn parse_method(raw: &str) -> Result<String> {
    let upper = raw.trim().to_ascii_uppercase();
    if HTTP_METHODS.contains(&upper.as_str()) {
        Ok(upper)
    } else {
        bail!(
            "unsupported HTTP method `{raw}` (expected one of {})",
            HTTP_METHODS.join("/")
        )
    }
}

impl WebFetcher {
    /// Send `req`, following up to [`MAX_REDIRECTS`] redirects. Before each
    /// hop `check_hop(method, url)` may stop the request; `credentials(url)`
    /// is merged into the headers of every request sent, the first included.
    /// A non-2xx status is a normal result, as for [`WebFetcher::fetch`].
    pub async fn request<C, Fut, A>(
        &self,
        req: HttpRequest,
        check_hop: C,
        credentials: A,
    ) -> Result<FetchOutcome>
    where
        C: FnMut(String, String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
        A: Fn(&str) -> Vec<(String, String)>,
    {
        let max_bytes = req.max_bytes.clamp(1, HARD_MAX_BYTES);
        tokio::time::timeout(
            TOTAL_TIMEOUT,
            self.request_inner(req, max_bytes, check_hop, credentials),
        )
        .await
        .map_err(|_| anyhow::anyhow!("request timed out after {TOTAL_TIMEOUT:?}"))?
    }

    async fn request_inner<C, Fut, A>(
        &self,
        req: HttpRequest,
        max_bytes: usize,
        mut check_hop: C,
        credentials: A,
    ) -> Result<FetchOutcome>
    where
        C: FnMut(String, String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
        A: Fn(&str) -> Vec<(String, String)>,
    {
        let mut method = Method::from_bytes(parse_method(&req.method)?.as_bytes())
            .context("invalid HTTP method")?;
        let mut body = req.body;
        let mut url = parse_normalized(&req.url)?;
        for _ in 0..=MAX_REDIRECTS {
            ssrf::validate_url(&url, &self.guard)?;
            let mut builder = self.client.request(method.clone(), url.clone()).header(
                USER_AGENT,
                concat!("entanglement-web-fetch/", env!("CARGO_PKG_VERSION")),
            );
            for (name, value) in req.headers.iter().chain(&credentials(url.as_str())) {
                builder = builder.header(header_name(name)?, header_value(name, value)?);
            }
            if let Some(b) = &body {
                builder = builder.body(b.clone());
            }
            let response = builder.send().await.map_err(|e| {
                anyhow::anyhow!("requesting {}: {}", redacted(&url), error_chain(&e))
            })?;
            if response.status().is_redirection() {
                if let Some(next) = redirect_target(&url, &response)? {
                    // 303 always, and 301/302 for anything but GET/HEAD, turn
                    // into a body-less GET — what every browser does.
                    if response.status() == StatusCode::SEE_OTHER
                        || (matches!(
                            response.status(),
                            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND
                        ) && method != Method::GET
                            && method != Method::HEAD)
                    {
                        method = Method::GET;
                        body = None;
                    }
                    check_hop(method.to_string(), next.to_string()).await?;
                    url = next;
                    continue;
                }
            }
            return read_body(url, response, max_bytes).await;
        }
        bail!("too many redirects (more than {MAX_REDIRECTS})")
    }
}

/// `url` without its query string, for error text: a query can carry a
/// token the caller put there itself.
fn redacted(url: &Url) -> String {
    let mut shown = url.clone();
    shown.set_query(None);
    shown.to_string()
}

fn header_name(name: &str) -> Result<HeaderName> {
    HeaderName::from_bytes(name.as_bytes()).with_context(|| format!("invalid header name `{name}`"))
}

/// The value never appears in the error: it may be a credential.
fn header_value(name: &str, value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).with_context(|| format!("invalid value for header `{name}`"))
}
