//! Token endpoint operations and the refreshing token source (ADR-0153).
//!
//! Covers the three token-endpoint interactions the flow needs — exchanging an
//! authorization code, refreshing an expired access token, and RFC 7009
//! revocation — plus [`StoredTokenSource`], the [`AccessTokenSource`] the HTTP
//! transport consults on every request.
//!
//! **Never log a token value.** Errors here quote the endpoint and the OAuth
//! `error`/`error_description` fields, never the credential itself.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{AccessTokenSource, StoredAuth, TokenSet, TokenStore};

/// Refresh this many seconds before the recorded expiry, so a token doesn't
/// expire in flight between our check and the server's.
const EXPIRY_SKEW_SECS: u64 = 60;

/// The token endpoint's success response (RFC 6749 §5.1).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

impl TokenResponse {
    /// Fold into a [`TokenSet`], carrying `previous`'s refresh token when the
    /// response omits one — a server that does not rotate refresh tokens simply
    /// leaves the field out, and dropping it would strand the grant.
    fn into_token_set(self, previous: Option<&TokenSet>) -> TokenSet {
        TokenSet {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
            token_type: self.token_type.unwrap_or_else(|| "Bearer".to_string()),
            expires_at: self.expires_in.map(|s| now_secs().saturating_add(s)),
            scope: self
                .scope
                .or_else(|| previous.and_then(|p| p.scope.clone())),
        }
    }
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Exchange an authorization code for tokens (RFC 6749 §4.1.3 + PKCE §4.5).
pub async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client: &super::ClientRegistration,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    resource: Option<&str>,
) -> Result<TokenSet> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client.client_id.as_str()),
        ("code_verifier", code_verifier),
    ];
    if let Some(r) = resource {
        form.push(("resource", r));
    }
    post_token(
        http,
        token_endpoint,
        client.client_secret.as_deref(),
        &form,
        None,
    )
    .await
}

/// Refresh an access token (RFC 6749 §6).
pub async fn refresh_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    previous: &TokenSet,
    resource: Option<&str>,
) -> Result<TokenSet> {
    let Some(refresh) = previous.refresh_token.as_deref() else {
        bail!("stored token has no refresh token — re-authorization is required");
    };
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", client_id),
    ];
    if let Some(r) = resource {
        form.push(("resource", r));
    }
    post_token(http, token_endpoint, client_secret, &form, Some(previous)).await
}

/// Best-effort RFC 7009 revocation. A server that rejects the request still
/// leaves the caller free to drop the token locally — the caller decides.
pub async fn revoke(
    http: &reqwest::Client,
    revocation_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    token: &str,
    token_type_hint: &str,
) -> Result<()> {
    let form = vec![
        ("token", token),
        ("token_type_hint", token_type_hint),
        ("client_id", client_id),
    ];
    let mut req = http.post(revocation_endpoint).form(&form);
    if let Some(secret) = client_secret {
        req = req.basic_auth(client_id, Some(secret));
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("POST {revocation_endpoint}"))?;
    if !resp.status().is_success() {
        bail!("revocation endpoint returned HTTP {}", resp.status());
    }
    Ok(())
}

/// `POST` a form to the token endpoint and parse the result, mapping an OAuth
/// error body to a readable message.
async fn post_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_secret: Option<&str>,
    form: &[(&str, &str)],
    previous: Option<&TokenSet>,
) -> Result<TokenSet> {
    let mut req = http.post(token_endpoint).form(form);
    if let Some(secret) = client_secret {
        // A confidential client authenticates with HTTP Basic (RFC 6749 §2.3.1).
        let client_id = form
            .iter()
            .find(|(k, _)| *k == "client_id")
            .map(|(_, v)| *v)
            .unwrap_or_default();
        req = req.basic_auth(client_id, Some(secret));
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("POST {token_endpoint}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "token endpoint returned HTTP {status}: {}",
            oauth_error(&body)
        );
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .with_context(|| format!("parsing the token response from {token_endpoint}"))?;
    Ok(parsed.into_token_set(previous))
}

/// Render an OAuth error body (RFC 6749 §5.2) as `error: description`, falling
/// back to the raw body when it isn't the standard shape. Token values never
/// appear in an error body, so this is safe to surface.
fn oauth_error(body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: String,
        #[serde(default)]
        error_description: Option<String>,
    }
    match serde_json::from_str::<ErrorBody>(body) {
        Ok(e) => match e.error_description {
            Some(d) => format!("{}: {d}", e.error),
            None => e.error,
        },
        Err(_) => body.chars().take(200).collect(),
    }
}

/// The [`AccessTokenSource`] the HTTP transport uses: reads the persisted token
/// for one server and refreshes it when expired (or when the transport forces a
/// refresh after a `401`).
///
/// Refreshes are serialized per source by an async mutex so two concurrent MCP
/// requests can't both spend a single-use rotating refresh token. Across
/// *processes* the store's own file lock serializes the write but not the
/// exchange — two `skutter` instances refreshing the same rotating grant at the
/// same instant can still have one lose. The loser recovers by re-authorizing;
/// a cross-process refresh lease is deliberately out of scope for v1.
pub struct StoredTokenSource {
    server: String,
    store: Arc<dyn TokenStore>,
    http: reqwest::Client,
    refresh_lock: tokio::sync::Mutex<()>,
}

impl StoredTokenSource {
    /// Everything but the server name is read back out of the store on each
    /// call, so a token another process refreshed (or a re-`connect` that
    /// re-registered the client) is picked up without rebuilding the source.
    ///
    /// The HTTP client is built here rather than passed in: the runtime supplies
    /// policy (which store, which server) and this crate owns every network
    /// detail, so the head never has to name `reqwest` (ADR-0153).
    pub fn new(server: impl Into<String>, store: Arc<dyn TokenStore>) -> Self {
        Self {
            server: server.into(),
            store,
            http: super::http_client(),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }
}

#[async_trait]
impl AccessTokenSource for StoredTokenSource {
    async fn access_token(&self, force_refresh: bool) -> Result<String> {
        let _guard = self.refresh_lock.lock().await;
        let current = self
            .store
            .load(&self.server)
            .with_context(|| format!("reading the stored token for `{}`", self.server))?;
        let Some(current) = current else {
            bail!(
                "no stored token for MCP server `{}` — run `/mcp connect {}`",
                self.server,
                self.server
            );
        };
        if !force_refresh && !current.tokens.is_expired(EXPIRY_SKEW_SECS) {
            return Ok(current.tokens.access_token);
        }
        let refreshed = refresh_token(
            &self.http,
            &current.token_endpoint,
            &current.client_id,
            current.client_secret.as_deref(),
            &current.tokens,
            current.resource.as_deref(),
        )
        .await
        .with_context(|| format!("refreshing the token for `{}`", self.server))?;
        let updated = StoredAuth {
            tokens: refreshed,
            ..current
        };
        self.store
            .save(&self.server, &updated)
            .with_context(|| format!("persisting the refreshed token for `{}`", self.server))?;
        Ok(updated.tokens.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_without_refresh_token_keeps_the_previous_one() {
        let previous = TokenSet {
            access_token: "old".into(),
            refresh_token: Some("keep-me".into()),
            token_type: "Bearer".into(),
            expires_at: Some(0),
            scope: Some("read".into()),
        };
        let resp = TokenResponse {
            access_token: "new".into(),
            token_type: None,
            expires_in: Some(3600),
            refresh_token: None,
            scope: None,
        };
        let got = resp.into_token_set(Some(&previous));
        assert_eq!(got.access_token, "new");
        assert_eq!(got.refresh_token.as_deref(), Some("keep-me"));
        assert_eq!(got.scope.as_deref(), Some("read"));
        // Defaults to Bearer when the server omits the type.
        assert_eq!(got.token_type, "Bearer");
        assert!(got.expires_at.unwrap() > now_secs());
    }

    #[test]
    fn rotated_refresh_token_replaces_the_previous_one() {
        let previous = TokenSet {
            access_token: "old".into(),
            refresh_token: Some("old-refresh".into()),
            token_type: "Bearer".into(),
            expires_at: None,
            scope: None,
        };
        let resp = TokenResponse {
            access_token: "new".into(),
            token_type: Some("Bearer".into()),
            expires_in: None,
            refresh_token: Some("new-refresh".into()),
            scope: None,
        };
        let got = resp.into_token_set(Some(&previous));
        assert_eq!(got.refresh_token.as_deref(), Some("new-refresh"));
        // No `expires_in` → no recorded expiry → never treated as expired.
        assert!(got.expires_at.is_none());
        assert!(!got.is_expired(60));
    }

    #[test]
    fn oauth_error_renders_the_standard_shape() {
        assert_eq!(
            oauth_error(r#"{"error":"invalid_grant","error_description":"expired"}"#),
            "invalid_grant: expired"
        );
        assert_eq!(
            oauth_error(r#"{"error":"invalid_client"}"#),
            "invalid_client"
        );
        // Non-standard bodies fall back to a bounded excerpt.
        assert_eq!(oauth_error("<html>boom</html>"), "<html>boom</html>");
    }

    #[tokio::test]
    async fn refresh_without_a_refresh_token_is_a_clear_error() {
        let set = TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_at: Some(0),
            scope: None,
        };
        let err = refresh_token(
            &reqwest::Client::new(),
            "https://as.example/token",
            "cid",
            None,
            &set,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no refresh token"));
    }
}
