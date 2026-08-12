//! The web-redirect authorization flow for embedders (#684).
//!
//! Same authorization-code + PKCE grant as [`super::flow::AuthFlow`], but the
//! redirect lands on the *embedder's own* HTTPS callback endpoint instead of a
//! loopback listener this crate binds. That splits the flow across two of the
//! embedder's HTTP requests:
//!
//! 1. [`WebFlow::begin`] resolves endpoints, registers a client if needed, and
//!    hands back a [`PendingWebAuthorization`]. The embedder persists it keyed
//!    by [`PendingWebAuthorization::state`] (with a TTL — nothing here expires
//!    it) and redirects the user's browser to
//!    [`PendingWebAuthorization::authorize_url`].
//! 2. Its callback handler receives `code` + `state`, loads (and deletes) the
//!    pending entry, and calls [`PendingWebAuthorization::complete`], saving
//!    the returned [`StoredAuth`] into its per-user token store
//!    ([`super::user_scoped`] / [`super::UserTokenStore`], ADR-0184).
//!
//! Nothing binds, nothing blocks on the user, and no server is spawned — the
//! embedder's web framework catches the redirect. `cfg.redirect_port` is
//! ignored: the redirect URI is the caller's, verbatim.
//!
//! **The pending state is `Serialize`/`Deserialize` on purpose**: a
//! multi-replica embedder cannot guarantee the callback lands on the replica
//! that ran `begin`, so the pending entry must round-trip through its shared
//! store. That store then briefly holds the PKCE verifier and any
//! `client_secret` — acceptable, because the same store holds strictly more
//! sensitive material long-term (`StoredAuth` carries the `client_secret` and
//! refresh tokens), and the verifier is single-use and worthless without the
//! state-bound authorization code. The `Debug` impl still redacts both.

use std::fmt;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::flow::prepare;
use super::{token, ClientRegistration, OauthConfig, StoredAuth};

/// Entry point for starting a web-redirect authorization.
pub struct WebFlow;

impl WebFlow {
    /// Like [`super::flow::AuthFlow::begin`], but the redirect lands on
    /// `redirect_uri` — the embedder's own callback endpoint — and the consent
    /// screen shows `client_name` (the embedder's product, not "skutter").
    /// Performs network I/O (discovery, registration) but never blocks on the
    /// user and binds nothing.
    ///
    /// With no pre-issued `cfg.client_id`, every `begin` mints a fresh client
    /// via dynamic registration; a deployment wanting one stable client per
    /// authorization server supplies `client_id` in its `OauthConfig`.
    pub async fn begin(
        server: &str,
        mcp_url: &str,
        cfg: &OauthConfig,
        resource_metadata_hint: Option<&str>,
        redirect_uri: &str,
        client_name: &str,
    ) -> Result<PendingWebAuthorization> {
        let prepared = prepare(
            server,
            mcp_url,
            cfg,
            resource_metadata_hint,
            redirect_uri,
            client_name,
        )
        .await?;
        Ok(PendingWebAuthorization {
            server: server.to_string(),
            authorize_url: prepared.authorize_url,
            redirect_uri: redirect_uri.to_string(),
            state: prepared.state,
            pkce_verifier: prepared.pkce.verifier,
            client_id: prepared.client.client_id,
            client_secret: prepared.client.client_secret,
            token_endpoint: prepared.endpoints.token_endpoint,
            revocation_endpoint: prepared.endpoints.revocation_endpoint,
            resource: prepared.endpoints.resource,
        })
    }
}

/// An authorization waiting on the embedder's callback request. Plain data —
/// see the module docs for the serializability contract and what it puts in
/// the embedder's store.
#[derive(Clone, Serialize, Deserialize)]
pub struct PendingWebAuthorization {
    server: String,
    authorize_url: String,
    redirect_uri: String,
    state: String,
    pkce_verifier: String,
    client_id: String,
    client_secret: Option<String>,
    token_endpoint: String,
    revocation_endpoint: Option<String>,
    resource: Option<String>,
}

impl PendingWebAuthorization {
    /// The URL to send the user's browser to.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// The CSRF token carried through the round-trip — the embedder's natural
    /// storage key for this pending entry.
    pub fn state(&self) -> &str {
        &self.state
    }

    /// The callback URI this authorization was begun with.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The server name this authorization is for.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Verify the presented `state`, then exchange the code for tokens. A
    /// state mismatch is rejected before any network I/O. The returned
    /// [`StoredAuth`] carries everything needed to refresh later without
    /// re-running discovery.
    pub async fn complete(self, code: &str, state: &str) -> Result<StoredAuth> {
        if state != self.state {
            bail!(
                "state mismatch completing the authorization for `{}` — possible CSRF \
                 or a stale/replayed callback",
                self.server
            );
        }
        let http = super::http_client();
        let client = ClientRegistration {
            client_id: self.client_id,
            client_secret: self.client_secret,
        };
        let tokens = token::exchange_code(
            &http,
            &self.token_endpoint,
            &client,
            code,
            &self.redirect_uri,
            &self.pkce_verifier,
            self.resource.as_deref(),
        )
        .await
        .with_context(|| format!("exchanging the authorization code for `{}`", self.server))?;
        Ok(StoredAuth {
            client_id: client.client_id,
            client_secret: client.client_secret,
            token_endpoint: self.token_endpoint,
            revocation_endpoint: self.revocation_endpoint,
            resource: self.resource,
            tokens,
        })
    }
}

/// Redacts the PKCE verifier and client secret: the pending struct is exactly
/// the kind of value an embedder logs while debugging its callback handler.
impl fmt::Debug for PendingWebAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingWebAuthorization")
            .field("server", &self.server)
            .field("authorize_url", &self.authorize_url)
            .field("redirect_uri", &self.redirect_uri)
            .field("state", &self.state)
            .field("pkce_verifier", &"<redacted>")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("token_endpoint", &self.token_endpoint)
            .field("revocation_endpoint", &self.revocation_endpoint)
            .field("resource", &self.resource)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Overrides + a pre-issued client id short-circuit discovery and DCR, so
    /// `begin` makes zero network calls.
    fn offline_cfg() -> OauthConfig {
        OauthConfig {
            authorization_url: Some("https://as.example/authorize".into()),
            token_url: Some("https://192.0.2.1/token".into()),
            client_id: Some("pre-issued".into()),
            client_secret: Some("SECRET".into()),
            scopes: vec!["read".into()],
            ..OauthConfig::default()
        }
    }

    async fn offline_pending() -> PendingWebAuthorization {
        WebFlow::begin(
            "kb",
            "https://mcp.example/mcp",
            &offline_cfg(),
            None,
            "https://app.example/oauth/mcp/callback",
            "kb-app",
        )
        .await
        .expect("offline begin must not touch the network")
    }

    #[tokio::test]
    async fn begin_with_overrides_builds_the_url_and_needs_no_network() {
        let pending = offline_pending().await;
        let url = pending.authorize_url();
        assert!(url.starts_with("https://as.example/authorize?"));
        assert!(url.contains("client_id=pre-issued"));
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fapp.example%2Foauth%2Fmcp%2Fcallback"),
            "the caller's redirect URI must ride the authorize URL verbatim: {url}"
        );
        assert!(url.contains(&format!("state={}", pending.state())));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        // Base64url of 32 random bytes — same CSPRNG as the loopback flow.
        assert_eq!(pending.state().len(), 43);
        assert_eq!(pending.server(), "kb");
        assert_eq!(
            pending.redirect_uri(),
            "https://app.example/oauth/mcp/callback"
        );
    }

    #[tokio::test]
    async fn complete_rejects_a_mismatched_state_before_any_io() {
        let pending = offline_pending().await;
        // The token endpoint is non-routable (TEST-NET-1); a state mismatch
        // must fail fast without ever attempting it.
        let err = pending
            .complete("some-code", "wrong-state")
            .await
            .expect_err("mismatched state must be rejected");
        assert!(err.to_string().contains("state mismatch"), "{err:#}");
        assert!(err.to_string().contains("`kb`"), "{err:#}");
    }

    #[tokio::test]
    async fn debug_redacts_the_verifier_and_client_secret() {
        let pending = offline_pending().await;
        let rendered = format!("{pending:?}");
        assert!(!rendered.contains("SECRET"));
        assert!(!rendered.contains(&pending.pkce_verifier));
        assert!(rendered.contains("<redacted>"));
        // Non-secret context stays visible for debugging.
        assert!(rendered.contains("pre-issued"));
        assert!(rendered.contains("as.example"));
        assert!(rendered.contains(pending.state()));
    }

    /// Pins the documented contract that the pending entry round-trips through
    /// an embedder's store intact — including that the JSON *does* carry the
    /// PKCE verifier, so weakening that (and breaking `complete` after a
    /// replica handoff) is a conscious change, not an accident.
    #[tokio::test]
    async fn pending_serde_round_trip_preserves_completion_inputs() {
        let pending = offline_pending().await;
        let json = serde_json::to_string(&pending).expect("serialize");
        assert!(json.contains(&pending.pkce_verifier));
        let restored: PendingWebAuthorization = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.server, pending.server);
        assert_eq!(restored.authorize_url, pending.authorize_url);
        assert_eq!(restored.redirect_uri, pending.redirect_uri);
        assert_eq!(restored.state, pending.state);
        assert_eq!(restored.pkce_verifier, pending.pkce_verifier);
        assert_eq!(restored.client_id, pending.client_id);
        assert_eq!(restored.client_secret, pending.client_secret);
        assert_eq!(restored.token_endpoint, pending.token_endpoint);
        assert_eq!(restored.revocation_endpoint, pending.revocation_endpoint);
        assert_eq!(restored.resource, pending.resource);
    }
}
