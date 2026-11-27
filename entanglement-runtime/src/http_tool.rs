//! `http` — the tool behind the rhai `http(method, url, #{headers, body})`
//! binding (ADR-0227). Registered, never advertised: a script reaches it,
//! graded as `Exec` on `"{METHOD} {url}"` through the same dispatch seam as
//! every other binding, and it rides `web_fetch`'s SSRF-guarded client.
//!
//! Hidden credentials: `config.yml` `http_auth:` (user layer only) maps a
//! host to headers/cookies, injected here at send time — over https, on the
//! original host only, so a redirect elsewhere goes out bare. The script can
//! neither see nor supply them (an `Authorization`/`Cookie` header of its own
//! is refused), the result is the body alone (no response headers, so no
//! `Set-Cookie` echo), and resolved values are scrubbed from error text. A
//! token a server echoes inside a body is out of reach — accepted.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use entanglement_core::web_fetch::{
    has_userinfo, scheme_and_host, FetchBody, FetchOutcome, HttpRequest, WebFetcher,
};
use entanglement_core::Permission;

use crate::capability::Capability;
use crate::http_gate::HttpGate;
use crate::tool_names::HTTP_TOOL;
use crate::tools::Tool;

/// Response bytes kept — a Rhai string is capped at 256 KiB anyway.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Headers a script may not set: the credential carriers only `http_auth`
/// may fill, and the host overrides that would let a request matched (and
/// credentialed) for one host be routed as another.
const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "proxy-authorization",
    "host",
    "x-forwarded-host",
];

pub struct HttpTool {
    fetcher: WebFetcher,
    gate: Arc<HttpGate>,
}

impl HttpTool {
    /// Crate-private: the executor builder registers it with the very gate
    /// the dispatch seam forces approvals through (`ToolExecutorBuilder::
    /// http_gate`) — two separately wired gates would inject unapproved.
    pub(crate) fn new(gate: Arc<HttpGate>) -> Result<Self> {
        Ok(Self {
            fetcher: WebFetcher::new()?,
            gate,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    method: String,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

#[async_trait]
impl Tool for HttpTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(HTTP_TOOL)
    }

    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Exec]
    }

    fn description(&self) -> &str {
        "Send one HTTP request (script binding `http`); returns the response body. \
         Credentials for the host come from the user's http_auth config, never the caller."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "description": "GET/HEAD/POST/PUT/PATCH/DELETE." },
                "url": { "type": "string", "description": "The http(s) URL." },
                "headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Extra request headers (not Authorization/Cookie)."
                },
                "body": { "type": "string", "description": "Request body." }
            },
            "required": ["method", "url"]
        })
    }

    async fn run(&self, input: &str) -> Result<String> {
        let input: Input = serde_json::from_str(input).context("invalid http arguments")?;
        if let Some(name) = input
            .headers
            .keys()
            .find(|k| RESERVED_HEADERS.contains(&k.to_ascii_lowercase().as_str()))
        {
            bail!(
                "http: a script may not set the `{name}` header — credentials come from \
                 http_auth in the user config, and the host from the URL"
            );
        }
        // Userinfo would be a second, script-chosen credential channel.
        if has_userinfo(&input.url) {
            bail!("http: a URL with embedded credentials (user@host) is refused");
        }
        let auth = match self.gate.entry_for_url(&input.url) {
            Some(entry) => entry
                .resolve(|var| std::env::var(var).ok())
                .map_err(|e| anyhow::anyhow!("http: {e}"))?,
            None => Default::default(),
        };
        let (credentials, secrets) = (auth.headers, auth.secrets);
        let origin = scheme_and_host(&input.url).map(|(_, host)| host);
        let req = HttpRequest {
            method: input.method,
            url: input.url,
            headers: input.headers.into_iter().collect(),
            body: input.body,
            max_bytes: MAX_BODY_BYTES,
        };
        let outcome = self
            .fetcher
            .request(req, check_hop, |url| {
                hop_credentials(&credentials, origin.as_deref(), url)
            })
            .await
            .map_err(|e| anyhow::anyhow!(scrub(&format!("{e:#}"), &secrets)))?;
        render(outcome, &secrets)
    }
}

/// The script-visible result: the body (or a binary placeholder); a non-2xx
/// status is an error whose text — body included — is scrubbed of every
/// secret value.
fn render(outcome: FetchOutcome, secrets: &[String]) -> Result<String> {
    let body = match outcome.body {
        FetchBody::Binary { size } => format!(
            "[binary omitted: {}, {} bytes]",
            outcome.mime.as_deref().unwrap_or("unknown"),
            size.map_or_else(|| "unknown".to_string(), |n| n.to_string())
        ),
        FetchBody::Text { text, truncated } if truncated => {
            format!("{text}\n[response cut at {MAX_BODY_BYTES} bytes]")
        }
        FetchBody::Text { text, .. } => text,
    };
    if !(200..300).contains(&outcome.status) {
        bail!(scrub(
            &format!("HTTP {} from {}: {body}", outcome.status, outcome.final_url),
            secrets
        ));
    }
    Ok(body)
}

/// The credentials a hop to `url` may carry: all of them over https to the
/// original host, none anywhere else.
fn hop_credentials(
    credentials: &[(String, String)],
    origin: Option<&str>,
    url: &str,
) -> Vec<(String, String)> {
    match (scheme_and_host(url), origin) {
        (Some((scheme, host)), Some(origin)) if scheme == "https" && host == origin => {
            credentials.to_vec()
        }
        _ => Vec::new(),
    }
}

/// `text` with every non-empty secret value replaced.
fn scrub(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|s| !s.is_empty())
        .fold(text.to_string(), |acc, s| {
            acc.replace(s.as_str(), "[redacted]")
        })
}

/// Grade a redirect hop as a fresh `http` call under the session's rules
/// (ADR-0225's `check_hop`, applied to `http`). `Ask` stops rather than
/// parking mid-request.
async fn check_hop(method: String, url: String) -> Result<()> {
    let input = serde_json::json!({ "method": method, "url": url }).to_string();
    match crate::tool_runner::grade_in_scope(HTTP_TOOL, &input).await {
        None | Some((Permission::Allow, _)) => Ok(()),
        Some((Permission::Deny, mode)) => {
            bail!("redirect to {url} not followed: `http` of that URL is denied by mode `{mode}`")
        }
        Some((Permission::Ask, _)) => bail!(
            "redirect to {url} not followed: requesting that URL needs approval — call \
             http on it directly to request it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Vec<(String, String)> {
        vec![("Authorization".into(), "Bearer s3cret".into())]
    }

    #[test]
    fn credentials_ride_only_https_hops_to_the_original_host() {
        let origin = Some("api.github.com");
        assert_eq!(
            hop_credentials(&creds(), origin, "https://api.github.com/next"),
            creds()
        );
        assert!(hop_credentials(&creds(), origin, "https://evil.example/x").is_empty());
        assert!(hop_credentials(&creds(), origin, "http://api.github.com/x").is_empty());
        assert!(hop_credentials(&creds(), None, "https://api.github.com/").is_empty());
    }

    #[test]
    fn scrub_redacts_every_secret_value() {
        let out = scrub(
            "failed: Bearer s3cret at s3cret",
            &["s3cret".into(), String::new()],
        );
        assert_eq!(out, "failed: Bearer [redacted] at [redacted]");
    }

    /// ADR-0228: inside a trusted / **Run all** script's call, an
    /// `Ask`-graded redirect hop is followed like the call itself was; a
    /// `Deny` hop still stops. Outside one, the `Ask` hop stops.
    #[tokio::test]
    async fn a_run_all_call_follows_an_ask_hop_but_never_a_deny_hop() {
        use crate::mode::{Limits, Mode, Rules};
        use crate::tool_runner::call_grader::{scope, test_support::grader_for};
        let mode = || Mode {
            name: "build".into(),
            default: Permission::Ask,
            rules: Rules::from_lists(&["http(GET https://evil.example/*)".into()], &[], &[]),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        };
        let hops = || async {
            (
                check_hop("GET".into(), "https://ok.example/x".into()).await,
                check_hop("GET".into(), "https://evil.example/x".into()).await,
            )
        };
        let mut grader = grader_for(mode(), crate::tools::ToolRegistry::new());
        grader.run_all = true;
        let (ask, deny) = scope(grader, hops()).await;
        assert!(ask.is_ok(), "{ask:?}");
        assert!(deny.unwrap_err().to_string().contains("denied"));
        let grader = grader_for(mode(), crate::tools::ToolRegistry::new());
        let (ask, _) = scope(grader, hops()).await;
        assert!(ask.unwrap_err().to_string().contains("needs approval"));
    }

    fn tool() -> HttpTool {
        let gate = HttpGate::new(
            serde_yaml::from_str(
                "example.com: {headers: {X-Key: \"${ENTANGLEMENT_TEST_UNSET_VAR}\"}}",
            )
            .unwrap(),
        );
        HttpTool::new(Arc::new(gate)).unwrap()
    }

    #[tokio::test]
    async fn a_script_supplied_credential_or_host_header_is_refused() {
        for name in [
            "Authorization",
            "cookie",
            "Proxy-Authorization",
            "Host",
            "X-Forwarded-Host",
        ] {
            let input = serde_json::json!({
                "method": "GET",
                "url": "https://example.com/",
                "headers": { name: "x" },
            });
            let err = tool().run(&input.to_string()).await.unwrap_err();
            assert!(format!("{err}").contains("may not set"), "{name}: {err}");
        }
    }

    #[tokio::test]
    async fn an_unset_credential_var_fails_naming_the_var() {
        let input = serde_json::json!({ "method": "GET", "url": "https://api.example.com/" });
        let err = tool().run(&input.to_string()).await.unwrap_err();
        assert!(
            format!("{err}").contains("ENTANGLEMENT_TEST_UNSET_VAR"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_url_with_userinfo_is_refused_before_anything_else() {
        let input = serde_json::json!({ "method": "GET", "url": "https://u:p@api.example.com/" });
        let err = tool().run(&input.to_string()).await.unwrap_err();
        assert!(format!("{err}").contains("embedded credentials"), "{err}");
        let input = serde_json::json!({ "method": "GET", "url": "https:u:p@api.example.com/" });
        let err = tool().run(&input.to_string()).await.unwrap_err();
        assert!(format!("{err}").contains("embedded credentials"), "{err}");
    }

    /// The runtime has no loopback bypass to fetch from a local server (the
    /// SSRF guard's test switch is provider-`cfg(test)`-only, by design), so
    /// the non-2xx rendering `run` ends with is driven directly: a body
    /// echoing the bare token comes back scrubbed.
    #[test]
    fn a_non_2xx_body_echoing_the_bare_token_is_scrubbed() {
        let entry: crate::config::http_auth::HttpAuthEntry =
            serde_yaml::from_str("headers: {Authorization: \"Bearer ${T}\"}").unwrap();
        let auth = entry.resolve(|_| Some("tok123".into())).unwrap();
        let outcome = FetchOutcome {
            final_url: "https://api.example.com/x".into(),
            status: 401,
            mime: Some("text/plain".into()),
            body: FetchBody::Text {
                text: "bad token tok123".into(),
                truncated: false,
            },
        };
        let err = render(outcome, &auth.secrets).unwrap_err().to_string();
        assert!(!err.contains("tok123"), "{err}");
        assert!(
            err.contains("HTTP 401") && err.contains("[redacted]"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_ssrf_guard_still_applies() {
        let input = serde_json::json!({ "method": "GET", "url": "http://127.0.0.1:9/" });
        assert!(tool().run(&input.to_string()).await.is_err());
    }
}
