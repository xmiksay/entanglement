//! OAuth metadata discovery for MCP servers (ADR-0153).
//!
//! Two RFCs chained together let a client authenticate against a server it was
//! given nothing but a URL for — which is what makes remote MCP servers usable
//! without hand-pasting endpoints into `config.yml`:
//!
//! 1. **RFC 9728** (protected-resource metadata) — the `401` challenge's
//!    `WWW-Authenticate: Bearer resource_metadata="…"` parameter points at a JSON
//!    document naming the server's authorization server(s). With no challenge to
//!    read, the well-known path is derived from the MCP URL itself.
//! 2. **RFC 8414** (authorization-server metadata) — that issuer's own document
//!    supplies the authorization/token/registration/revocation endpoints.
//!
//! Every step is overridable: a user who sets `authorization_url` + `token_url`
//! in the `oauth:` block short-circuits discovery entirely, which is the escape
//! hatch for a server that publishes no metadata.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Endpoints resolved for one server — the union of what discovery found and
/// what the user's `oauth:` block overrode.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    /// The `resource` identifier to pass as RFC 8707 `resource` on the
    /// authorization and token requests, when the resource metadata named one.
    pub resource: Option<String>,
    pub scopes_supported: Vec<String>,
}

/// RFC 9728 protected-resource metadata (only the fields we act on).
#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// RFC 8414 authorization-server metadata (only the fields we act on).
#[derive(Debug, Deserialize)]
struct AuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    revocation_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

/// Resolve every endpoint needed to authorize against `mcp_url`.
///
/// `resource_metadata_hint` is the `resource_metadata` parameter lifted from a
/// `401` challenge, when one was seen. Discovery is skipped entirely when the
/// caller already supplies both an authorization and a token endpoint.
pub async fn discover(
    http: &reqwest::Client,
    mcp_url: &str,
    resource_metadata_hint: Option<&str>,
    override_authorization: Option<&str>,
    override_token: Option<&str>,
    override_registration: Option<&str>,
) -> Result<Endpoints> {
    // Fully-overridden: no network calls at all.
    if let (Some(auth), Some(token)) = (override_authorization, override_token) {
        return Ok(Endpoints {
            authorization_endpoint: auth.to_string(),
            token_endpoint: token.to_string(),
            registration_endpoint: override_registration.map(str::to_string),
            revocation_endpoint: None,
            resource: None,
            scopes_supported: Vec::new(),
        });
    }

    let resource_meta = fetch_resource_metadata(http, mcp_url, resource_metadata_hint).await;
    let (issuers, resource, resource_scopes) = match resource_meta {
        Some(m) => (m.authorization_servers, m.resource, m.scopes_supported),
        // No protected-resource document: many deployments still serve AS
        // metadata at the MCP server's own origin, so try that as the issuer.
        None => (vec![origin_of(mcp_url)?], None, Vec::new()),
    };
    if issuers.is_empty() {
        bail!("protected-resource metadata names no authorization server");
    }

    let mut last_err = None;
    for issuer in &issuers {
        match fetch_as_metadata(http, issuer).await {
            Ok(meta) => {
                if !meta.code_challenge_methods_supported.is_empty()
                    && !meta
                        .code_challenge_methods_supported
                        .iter()
                        .any(|m| m == super::pkce::Pkce::method())
                {
                    bail!(
                        "authorization server `{issuer}` does not support PKCE S256 \
                         (advertises {:?})",
                        meta.code_challenge_methods_supported
                    );
                }
                let scopes = if resource_scopes.is_empty() {
                    meta.scopes_supported
                } else {
                    resource_scopes
                };
                return Ok(Endpoints {
                    authorization_endpoint: override_authorization
                        .map(str::to_string)
                        .unwrap_or(meta.authorization_endpoint),
                    token_endpoint: override_token
                        .map(str::to_string)
                        .unwrap_or(meta.token_endpoint),
                    registration_endpoint: override_registration
                        .map(str::to_string)
                        .or(meta.registration_endpoint),
                    revocation_endpoint: meta.revocation_endpoint,
                    resource,
                    scopes_supported: scopes,
                });
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no authorization-server metadata found")))
}

/// Fetch the RFC 9728 document, preferring the `401` challenge's pointer and
/// falling back to the well-known paths derived from the MCP URL. Returns `None`
/// when nothing is published — not an error, since the AS-at-origin fallback is
/// still worth trying.
async fn fetch_resource_metadata(
    http: &reqwest::Client,
    mcp_url: &str,
    hint: Option<&str>,
) -> Option<ProtectedResourceMetadata> {
    let mut candidates = Vec::new();
    if let Some(h) = hint {
        candidates.push(h.to_string());
    }
    candidates.extend(well_known_candidates(mcp_url, "oauth-protected-resource"));
    for url in candidates {
        if let Ok(resp) = http.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(meta) = resp.json::<ProtectedResourceMetadata>().await {
                    return Some(meta);
                }
            }
        }
    }
    None
}

/// Fetch RFC 8414 authorization-server metadata for `issuer`, trying the
/// OAuth well-known path and then the OpenID Connect one.
async fn fetch_as_metadata(http: &reqwest::Client, issuer: &str) -> Result<AuthServerMetadata> {
    let mut candidates = well_known_candidates(issuer, "oauth-authorization-server");
    candidates.extend(well_known_candidates(issuer, "openid-configuration"));
    let mut last = None;
    for url in &candidates {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<AuthServerMetadata>().await
            {
                Ok(meta) => return Ok(meta),
                Err(e) => last = Some(anyhow::Error::new(e).context(format!("parsing {url}"))),
            },
            Ok(resp) => {
                last = Some(anyhow::anyhow!("{url} returned HTTP {}", resp.status()));
            }
            Err(e) => last = Some(anyhow::Error::new(e).context(format!("fetching {url}"))),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no metadata candidates for `{issuer}`")))
}

/// The well-known URLs to try for `suffix`, per RFC 8414 §3.1 / RFC 9728 §3.1:
/// the path-aware form (`/.well-known/<suffix><path>`) first, then the plain
/// origin form. A URL with no path yields just the latter.
fn well_known_candidates(base: &str, suffix: &str) -> Vec<String> {
    let Ok(origin) = origin_of(base) else {
        return Vec::new();
    };
    let path = path_of(base);
    let mut out = Vec::new();
    if !path.is_empty() && path != "/" {
        out.push(format!(
            "{origin}/.well-known/{suffix}{}",
            path.trim_end_matches('/')
        ));
    }
    out.push(format!("{origin}/.well-known/{suffix}"));
    out
}

/// `https://host:port` from a URL, without the path. Hand-rolled rather than
/// pulling a `url` crate for two string splits.
fn origin_of(url: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("`{url}` is not an absolute URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        bail!("`{url}` has no host");
    }
    Ok(format!("{scheme}://{authority}"))
}

/// The path component of a URL (`""` when there is none), query/fragment stripped.
fn path_of(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return String::new();
    };
    match rest.find('/') {
        Some(i) => {
            let path = &rest[i..];
            path.split(['?', '#']).next().unwrap_or(path).to_string()
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_strips_path_query_and_fragment() {
        assert_eq!(
            origin_of("https://ex.com:8443/mcp?x=1#y").unwrap(),
            "https://ex.com:8443"
        );
        assert_eq!(
            origin_of("http://localhost/a/b").unwrap(),
            "http://localhost"
        );
        assert_eq!(origin_of("https://ex.com").unwrap(), "https://ex.com");
    }

    #[test]
    fn origin_rejects_a_non_absolute_url() {
        assert!(origin_of("/mcp").is_err());
        assert!(origin_of("https://").is_err());
    }

    #[test]
    fn path_extraction_drops_query_and_fragment() {
        assert_eq!(path_of("https://ex.com/mcp/v1?a=b"), "/mcp/v1");
        assert_eq!(path_of("https://ex.com"), "");
        assert_eq!(path_of("https://ex.com/"), "/");
    }

    #[test]
    fn well_known_tries_path_aware_form_first() {
        let got = well_known_candidates("https://ex.com/mcp", "oauth-protected-resource");
        assert_eq!(
            got,
            vec![
                "https://ex.com/.well-known/oauth-protected-resource/mcp".to_string(),
                "https://ex.com/.well-known/oauth-protected-resource".to_string(),
            ]
        );
    }

    #[test]
    fn well_known_for_a_bare_origin_is_just_the_plain_form() {
        let got = well_known_candidates("https://ex.com", "openid-configuration");
        assert_eq!(
            got,
            vec!["https://ex.com/.well-known/openid-configuration".to_string()]
        );
    }

    #[tokio::test]
    async fn full_override_short_circuits_discovery() {
        // An unroutable base URL proves no network call is attempted.
        let http = reqwest::Client::new();
        let got = discover(
            &http,
            "https://192.0.2.1/mcp",
            None,
            Some("https://as.example/authorize"),
            Some("https://as.example/token"),
            Some("https://as.example/register"),
        )
        .await
        .unwrap();
        assert_eq!(got.authorization_endpoint, "https://as.example/authorize");
        assert_eq!(got.token_endpoint, "https://as.example/token");
        assert_eq!(
            got.registration_endpoint.as_deref(),
            Some("https://as.example/register")
        );
    }
}
