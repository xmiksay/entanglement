//! OAuth 2.1 authentication for MCP servers (ADR-0153).
//!
//! The *mechanism* half of MCP OAuth: everything needed to turn a bare server
//! URL into a bearer token, and to keep that token fresh. The *policy* half —
//! where the token file lives, which servers are configured, who is allowed to
//! start a flow, and how the browser gets opened — stays in
//! `entanglement-runtime`, which supplies a [`TokenStore`] implementation and
//! drives [`flow`].
//!
//! The shape of one authorization:
//!
//! 1. [`discovery::discover`] resolves the authorization/token/registration
//!    endpoints from RFC 9728 + RFC 8414 metadata (or the user's overrides).
//! 2. [`dcr::register`] mints a `client_id` via RFC 7591 dynamic client
//!    registration when the config supplies none — the common case for remote
//!    MCP servers, which hand out no pre-issued client ids.
//! 3. [`flow::AuthFlow`] generates PKCE, binds a loopback redirect listener,
//!    and hands back the authorization URL for the caller to open.
//! 4. [`token::exchange_code`] trades the returned code for a [`TokenSet`],
//!    persisted as a [`StoredAuth`] together with the context needed to refresh
//!    it later without re-running discovery.
//! 5. [`token::StoredTokenSource`] serves that token to the HTTP transport,
//!    refreshing on expiry or on a forced retry after a `401`.
//!
//! **Secrets never reach the log.** No token, refresh token, client secret, or
//! authorization code is ever `Debug`-printed or traced; the [`Debug`] impls on
//! [`TokenSet`] and [`StoredAuth`] redact them explicitly.

use std::fmt;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod dcr;
pub mod device;
pub mod discovery;
pub mod flow;
pub mod loopback;
pub mod pkce;
pub mod token;
pub mod user_store;

pub use dcr::ClientRegistration;
pub use device::{DeviceFlow, PendingDeviceAuthorization};
pub use discovery::Endpoints;
pub use flow::{AuthFlow, AuthOutcome, PendingAuthorization};
pub use pkce::Pkce;
pub use token::StoredTokenSource;
pub use user_store::{user_scoped, InMemoryUserTokenStore, UserTokenStore};

/// The optional `oauth:` block on an MCP server's config. Every field is an
/// *override* — with the block present but empty, discovery and dynamic client
/// registration resolve everything. Declared here (not in the runtime) for the
/// same reason [`McpServerState`][crate::provider_mcp::McpServerState] is: it is
/// data the mechanism defines and the runtime merely parses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthConfig {
    /// Skip discovery for the authorization endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    /// Skip discovery for the token endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
    /// Skip discovery for the RFC 7591 registration endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_url: Option<String>,
    /// Skip discovery for the RFC 8628 device-authorization endpoint (device-code
    /// flow only — `/mcp connect <name> --device-code`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_authorization_url: Option<String>,
    /// A pre-issued client id. Absent ⇒ dynamic client registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// A pre-issued client secret, for the rare confidential-client server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Scopes to request. Empty ⇒ whatever the metadata advertises.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Pin the loopback redirect port instead of taking an ephemeral one. Needed
    /// only by a server that requires a pre-registered static redirect URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_port: Option<u16>,
}

/// An OAuth credential set for one server.
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Unix seconds at which the access token expires. `None` ⇒ the server gave
    /// no lifetime, so the token is used until it is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

impl TokenSet {
    /// Has the access token expired, allowing `skew` seconds of headroom? A
    /// token with no recorded expiry is never considered expired — the only
    /// signal for it is the server's own `401`.
    pub fn is_expired(&self, skew: u64) -> bool {
        match self.expires_at {
            Some(at) => token::now_secs().saturating_add(skew) >= at,
            None => false,
        }
    }
}

/// Redacts every secret: a token set is routinely carried inside error contexts
/// and structs that *are* logged.
impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Everything persisted for one authenticated server: the credential plus the
/// context needed to refresh it. Storing the resolved endpoints and client id
/// alongside the token is what lets a *startup* connect skip discovery and
/// registration entirely — those run only during an explicit `/mcp connect`.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredAuth {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    /// RFC 8707 resource indicator, echoed on refresh when the server used one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    pub tokens: TokenSet,
}

impl fmt::Debug for StoredAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredAuth")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("token_endpoint", &self.token_endpoint)
            .field("revocation_endpoint", &self.revocation_endpoint)
            .field("resource", &self.resource)
            .field("tokens", &self.tokens)
            .finish()
    }
}

/// Persistence for [`StoredAuth`], implemented by the runtime over a managed
/// file. Synchronous: the backing store is a small local file, and keeping the
/// trait sync spares every implementor an async-trait indirection.
pub trait TokenStore: Send + Sync {
    fn load(&self, server: &str) -> Result<Option<StoredAuth>>;
    fn save(&self, server: &str, auth: &StoredAuth) -> Result<()>;
    fn delete(&self, server: &str) -> Result<()>;

    /// Run `f` inside a single cross-process critical section scoped to
    /// `server`'s credential: `f` is handed whatever is currently stored (or
    /// `None`), and returns the [`StoredAuth`] to persist. This is what lets a
    /// token *refresh* — not just the eventual file write — be serialized across
    /// `skutter` processes: a loser that acquires the section second sees the
    /// winner's already-refreshed token via its `current` argument and can skip
    /// its own exchange entirely, closing the race ADR-0153 accepted for v1
    /// (#631).
    ///
    /// The default just chains `load` then `save` with no additional locking —
    /// correct for an embedder whose `TokenStore` isn't shared across processes.
    fn with_exclusive(
        &self,
        server: &str,
        f: Box<dyn FnOnce(Option<StoredAuth>) -> Result<StoredAuth> + '_>,
    ) -> Result<StoredAuth> {
        let current = self.load(server)?;
        let updated = f(current)?;
        self.save(server, &updated)?;
        Ok(updated)
    }
}

/// Supplies a bearer token to the HTTP transport, refreshing it as needed.
/// `force_refresh` is set when the transport retries after a `401`.
#[async_trait]
pub trait AccessTokenSource: Send + Sync {
    async fn access_token(&self, force_refresh: bool) -> Result<String>;
}

/// The error a transport returns when a request needs authorization it doesn't
/// have. Distinct from a generic transport failure so the runtime can report
/// "needs auth" (and prompt for `/mcp connect`) instead of "server unreachable".
#[derive(Debug, thiserror::Error)]
#[error("MCP server `{server}` requires authorization")]
pub struct AuthRequired {
    pub server: String,
    /// The RFC 9728 `resource_metadata` pointer from the `WWW-Authenticate`
    /// challenge, when the server sent one — the discovery starting point.
    pub resource_metadata: Option<String>,
}

/// The HTTP client every auth operation uses. Built here — not handed in — so
/// the runtime drives OAuth without naming `reqwest` (ADR-0153). These are
/// short, infrequent requests to a handful of endpoints, so they want none of
/// the LLM client's pooling/retry machinery.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        // The builder only fails on a broken TLS backend, which would break
        // every other request too; a default client keeps the error at the
        // request site where it can be reported usefully.
        .unwrap_or_default()
}

/// Report on a server's stored authorization, refreshing an expired token when
/// a refresh token is available.
///
/// This is `/mcp check`: it answers "would a request work right now?" without
/// opening a browser. A refresh that succeeds is persisted, so a `check` also
/// repairs a merely-stale token. Takes an `Arc` (rather than `&dyn TokenStore`)
/// because the refresh runs through [`token::refresh_locked`], which hands the
/// store off to a `spawn_blocking` task (#631).
pub async fn check(store: std::sync::Arc<dyn TokenStore>, server: &str) -> Result<AuthOutcome> {
    let Some(current) = store.load(server)? else {
        return Ok(AuthOutcome::NotAuthorized);
    };
    if !current.tokens.is_expired(0) {
        return Ok(AuthOutcome::AlreadyValid);
    }
    let http = http_client();
    token::refresh_locked(server.to_string(), store, http, false)
        .await
        .with_context(|| format!("refreshing the stored token for `{server}`"))?;
    Ok(AuthOutcome::Refreshed)
}

/// Drop a server's stored authorization, revoking it upstream first when the
/// authorization server advertised a revocation endpoint (RFC 7009).
///
/// Revocation is best-effort by design: if it fails, the local credential is
/// still deleted. Leaving a token on disk because the server was unreachable
/// would be the worse outcome.
pub async fn disconnect(store: &dyn TokenStore, server: &str) -> Result<AuthOutcome> {
    let Some(current) = store.load(server)? else {
        return Ok(AuthOutcome::NotAuthorized);
    };
    if let Some(endpoint) = current.revocation_endpoint.as_deref() {
        let http = http_client();
        // Revoke the refresh token when present: revoking the grant root takes
        // the access token with it on a conforming server.
        let (token_value, hint) = match current.tokens.refresh_token.as_deref() {
            Some(rt) => (rt, "refresh_token"),
            None => (current.tokens.access_token.as_str(), "access_token"),
        };
        if let Err(e) = token::revoke(
            &http,
            endpoint,
            &current.client_id,
            current.client_secret.as_deref(),
            token_value,
            hint,
        )
        .await
        {
            tracing::warn!("could not revoke the token for MCP server `{server}`: {e:#}");
        }
    }
    store.delete(server)?;
    Ok(AuthOutcome::Disconnected)
}

/// Does this error chain carry an [`AuthRequired`]? The runtime uses this to
/// classify a failed connect as "needs auth" rather than a transport fault.
pub fn is_auth_required(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<AuthRequired>())
}

/// The [`AuthRequired`] in an error chain, if any — carries the discovery hint.
pub fn auth_required_of(err: &anyhow::Error) -> Option<&AuthRequired> {
    err.chain().find_map(|e| e.downcast_ref::<AuthRequired>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(expires_at: Option<u64>) -> TokenSet {
        TokenSet {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            token_type: "Bearer".into(),
            expires_at,
            scope: None,
        }
    }

    #[test]
    fn expiry_honours_the_skew_and_the_absent_case() {
        let now = token::now_secs();
        // Already past → expired.
        assert!(token(Some(now.saturating_sub(1))).is_expired(0));
        // Inside the skew window → treated as expired so we refresh early.
        assert!(token(Some(now + 30)).is_expired(60));
        // Comfortably ahead → still valid.
        assert!(!token(Some(now + 3600)).is_expired(60));
        // No recorded lifetime → never proactively expired.
        assert!(!token(None).is_expired(60));
    }

    #[test]
    fn debug_impls_redact_every_secret() {
        let auth = StoredAuth {
            client_id: "public-id".into(),
            client_secret: Some("SECRET".into()),
            token_endpoint: "https://as.example/token".into(),
            revocation_endpoint: None,
            resource: None,
            tokens: token(Some(1)),
        };
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("SECRET"));
        assert!(!rendered.contains("\"at\"") && !rendered.contains("\"rt\""));
        assert!(rendered.contains("<redacted>"));
        // Non-secret context stays visible for debugging.
        assert!(rendered.contains("public-id"));
        assert!(rendered.contains("as.example"));
    }

    #[test]
    fn auth_required_is_detected_through_a_context_chain() {
        let err = anyhow::Error::new(AuthRequired {
            server: "s".into(),
            resource_metadata: Some("https://ex/meta".into()),
        })
        .context("connecting")
        .context("startup");
        assert!(is_auth_required(&err));
        assert_eq!(
            auth_required_of(&err).unwrap().resource_metadata.as_deref(),
            Some("https://ex/meta")
        );

        let other = anyhow::anyhow!("connection refused").context("startup");
        assert!(!is_auth_required(&other));
        assert!(auth_required_of(&other).is_none());
    }

    #[test]
    fn token_set_deserializes_with_defaults() {
        let t: TokenSet = serde_json::from_str(r#"{"access_token":"a"}"#).unwrap();
        assert_eq!(t.token_type, "Bearer");
        assert!(t.refresh_token.is_none());
        assert!(t.expires_at.is_none());
    }

    #[test]
    fn oauth_config_rejects_unknown_keys() {
        assert!(serde_yaml::from_str::<OauthConfig>("client_id: abc").is_ok());
        assert!(serde_yaml::from_str::<OauthConfig>("clientid: abc").is_err());
    }

    /// A minimal in-memory [`TokenStore`] with no override of `with_exclusive`,
    /// so calling it exercises the trait's default chain-`load`-then-`save`
    /// path.
    struct FakeStore(std::sync::Mutex<std::collections::HashMap<String, StoredAuth>>);

    impl TokenStore for FakeStore {
        fn load(&self, server: &str) -> Result<Option<StoredAuth>> {
            Ok(self.0.lock().unwrap().get(server).cloned())
        }
        fn save(&self, server: &str, auth: &StoredAuth) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(server.to_string(), auth.clone());
            Ok(())
        }
        fn delete(&self, server: &str) -> Result<()> {
            self.0.lock().unwrap().remove(server);
            Ok(())
        }
    }

    #[test]
    fn default_with_exclusive_loads_calls_f_and_persists_the_result() {
        let store = FakeStore(std::sync::Mutex::new(std::collections::HashMap::new()));
        // No stored credential yet — `f` must see `None`.
        let result = store
            .with_exclusive(
                "srv",
                Box::new(|current| {
                    assert!(current.is_none());
                    Ok(StoredAuth {
                        client_id: "cid".into(),
                        client_secret: None,
                        token_endpoint: "https://as.example/token".into(),
                        revocation_endpoint: None,
                        resource: None,
                        tokens: token(Some(1)),
                    })
                }),
            )
            .unwrap();
        assert_eq!(result.client_id, "cid");
        // The default impl persisted it via `save`.
        assert_eq!(store.load("srv").unwrap().unwrap().client_id, "cid");

        // A second call must see what the first one just persisted.
        let result2 = store
            .with_exclusive(
                "srv",
                Box::new(|current| {
                    let mut current = current.expect("must see the first call's write");
                    current.client_id = "cid-2".into();
                    Ok(current)
                }),
            )
            .unwrap();
        assert_eq!(result2.client_id, "cid-2");
        assert_eq!(store.load("srv").unwrap().unwrap().client_id, "cid-2");
    }
}
