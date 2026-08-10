//! The authorization-code flow, start to finish (ADR-0153).
//!
//! Split in two on purpose. [`AuthFlow::begin`] does everything up to the point
//! a human is needed — discovery, registration, PKCE, binding the loopback
//! listener — and hands back a [`PendingAuthorization`] carrying the URL to
//! visit. The caller (the runtime) decides how that URL reaches the user:
//! printing it, opening a browser, or both. [`PendingAuthorization::complete`]
//! then waits for the redirect and exchanges the code.
//!
//! Keeping the browser launch on the caller's side is what lets this crate stay
//! free of process spawning, and lets a headless head fall back to printing the
//! URL with no separate code path.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use super::loopback;
use super::{dcr, discovery, pkce::Pkce, token, ClientRegistration, OauthConfig, StoredAuth};

/// The client name presented at dynamic registration and shown on the
/// authorization server's consent screen.
const CLIENT_NAME: &str = "skutter";

/// How long to wait for the user to finish authorizing in their browser before
/// giving up and releasing the listener.
pub const DEFAULT_AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(300);

/// What an auth operation did, for the event the runtime emits back to a head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// A fresh authorization completed and a token was stored.
    Authorized,
    /// The stored token was still valid; nothing was minted.
    AlreadyValid,
    /// The stored token was expired and a refresh renewed it.
    Refreshed,
    /// The stored token was dropped (and revoked, when the server supports it).
    Disconnected,
    /// No token is stored for this server.
    NotAuthorized,
}

impl AuthOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthOutcome::Authorized => "authorized",
            AuthOutcome::AlreadyValid => "already valid",
            AuthOutcome::Refreshed => "refreshed",
            AuthOutcome::Disconnected => "disconnected",
            AuthOutcome::NotAuthorized => "not authorized",
        }
    }
}

/// Entry point for starting an authorization.
pub struct AuthFlow;

impl AuthFlow {
    /// Resolve endpoints, register a client if needed, and prepare the
    /// authorization request. Performs network I/O (discovery, registration) but
    /// never blocks on the user.
    pub async fn begin(
        server: &str,
        mcp_url: &str,
        cfg: &OauthConfig,
        resource_metadata_hint: Option<&str>,
    ) -> Result<PendingAuthorization> {
        let http = super::http_client();
        let endpoints = discovery::discover(
            &http,
            mcp_url,
            resource_metadata_hint,
            cfg.authorization_url.as_deref(),
            cfg.token_url.as_deref(),
            cfg.registration_url.as_deref(),
            cfg.device_authorization_url.as_deref(),
        )
        .await
        .with_context(|| format!("discovering OAuth metadata for MCP server `{server}`"))?;

        // Bind before registering: the redirect URI is part of the registration
        // request, so the ephemeral port has to be known first.
        let listener = loopback::bind(cfg.redirect_port).await?;
        let port = listener
            .local_addr()
            .context("reading the redirect listener address")?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let scopes = if cfg.scopes.is_empty() {
            endpoints.scopes_supported.clone()
        } else {
            cfg.scopes.clone()
        };

        let client = match &cfg.client_id {
            Some(id) => ClientRegistration {
                client_id: id.clone(),
                client_secret: cfg.client_secret.clone(),
            },
            None => {
                let endpoint = endpoints
                    .registration_endpoint
                    .as_deref()
                    .with_context(|| {
                        format!(
                        "MCP server `{server}` advertises no dynamic-registration endpoint and \
                         no `client_id` is configured — set `oauth.client_id` in config.yml"
                    )
                    })?;
                dcr::register(
                    &http,
                    endpoint,
                    Some(&redirect_uri),
                    &["authorization_code", "refresh_token"],
                    &scopes,
                    CLIENT_NAME,
                )
                .await
                .with_context(|| format!("registering a client with `{server}`"))?
            }
        };

        let pkce = Pkce::generate();
        // `state` and the PKCE verifier come from the same CSPRNG.
        let state = super::pkce::random_token();
        let authorize_url = build_authorize_url(
            &endpoints.authorization_endpoint,
            &client.client_id,
            &redirect_uri,
            &scopes,
            &state,
            &pkce.challenge,
            endpoints.resource.as_deref(),
        );

        Ok(PendingAuthorization {
            http,
            server: server.to_string(),
            authorize_url,
            redirect_uri,
            listener,
            state,
            pkce,
            client,
            endpoints,
        })
    }
}

/// An authorization waiting on the user's browser.
pub struct PendingAuthorization {
    http: reqwest::Client,
    server: String,
    authorize_url: String,
    redirect_uri: String,
    listener: TcpListener,
    state: String,
    pkce: Pkce,
    client: ClientRegistration,
    endpoints: discovery::Endpoints,
}

impl PendingAuthorization {
    /// The URL the user must visit. Show it *and* open it — a headless session
    /// has no browser to open.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// The loopback URI the server will redirect to.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for the redirect, then exchange the code for tokens. The returned
    /// [`StoredAuth`] carries everything needed to refresh later without
    /// re-running discovery.
    pub async fn complete(self, timeout: Duration) -> Result<StoredAuth> {
        let code = tokio::time::timeout(
            timeout,
            loopback::wait_for_code(&self.listener, &self.state),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out after {}s waiting for the browser to complete authorization for `{}`",
                timeout.as_secs(),
                self.server
            )
        })?
        .with_context(|| format!("authorizing MCP server `{}`", self.server))?;

        let tokens = token::exchange_code(
            &self.http,
            &self.endpoints.token_endpoint,
            &self.client,
            &code,
            &self.redirect_uri,
            &self.pkce.verifier,
            self.endpoints.resource.as_deref(),
        )
        .await
        .with_context(|| format!("exchanging the authorization code for `{}`", self.server))?;

        Ok(StoredAuth {
            client_id: self.client.client_id,
            client_secret: self.client.client_secret,
            token_endpoint: self.endpoints.token_endpoint,
            revocation_endpoint: self.endpoints.revocation_endpoint,
            resource: self.endpoints.resource,
            tokens,
        })
    }
}

/// Build the authorization request URL (RFC 6749 §4.1.1 + PKCE §4.3).
fn build_authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    challenge: &str,
    resource: Option<&str>,
) -> String {
    let enc = loopback::percent_encode;
    let mut params = vec![
        "response_type=code".to_string(),
        format!("client_id={}", enc(client_id)),
        format!("redirect_uri={}", enc(redirect_uri)),
        format!("state={}", enc(state)),
        format!("code_challenge={}", enc(challenge)),
        format!("code_challenge_method={}", Pkce::method()),
    ];
    if !scopes.is_empty() {
        params.push(format!("scope={}", enc(&scopes.join(" "))));
    }
    if let Some(r) = resource {
        // RFC 8707: bind the token to this resource so it can't be replayed at
        // another server behind the same authorization server.
        params.push(format!("resource={}", enc(r)));
    }
    // An endpoint may already carry a query (rare, but legal).
    let sep = if authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{authorization_endpoint}{sep}{}", params.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_every_required_parameter() {
        let url = build_authorize_url(
            "https://as.example/authorize",
            "client 1",
            "http://127.0.0.1:5000/callback",
            &["read".into(), "write".into()],
            "st4te",
            "chal",
            None,
        );
        assert!(url.starts_with("https://as.example/authorize?"));
        assert!(url.contains("response_type=code"));
        // Values are percent-encoded.
        assert!(url.contains("client_id=client%201"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2Fcallback"));
        assert!(url.contains("scope=read%20write"));
        assert!(url.contains("state=st4te"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        // No resource indicator when none was discovered.
        assert!(!url.contains("resource="));
    }

    #[test]
    fn authorize_url_appends_to_an_existing_query_and_adds_resource() {
        let url = build_authorize_url(
            "https://as.example/authorize?tenant=acme",
            "cid",
            "http://127.0.0.1:1/callback",
            &[],
            "s",
            "c",
            Some("https://mcp.example/"),
        );
        assert!(url.contains("?tenant=acme&response_type=code"));
        assert!(url.contains("resource=https%3A%2F%2Fmcp.example%2F"));
        // Empty scope list omits the parameter entirely.
        assert!(!url.contains("scope="));
    }

    #[test]
    fn outcome_labels_are_stable() {
        assert_eq!(AuthOutcome::Authorized.as_str(), "authorized");
        assert_eq!(AuthOutcome::AlreadyValid.as_str(), "already valid");
        assert_eq!(AuthOutcome::Refreshed.as_str(), "refreshed");
        assert_eq!(AuthOutcome::Disconnected.as_str(), "disconnected");
        assert_eq!(AuthOutcome::NotAuthorized.as_str(), "not authorized");
    }
}
