//! Opt-in bearer-token authentication for the `serve` head (#674,
//! [ADR-0174]) — the wire half of multi-user mode ([ADR-0147]).
//!
//! Connection-scoped, not per-frame: the WS upgrade handshake presents
//! `Authorization: Bearer <token>`, a pluggable [`WireAuthenticator`] resolves
//! it to a [`UserId`], and that binding lives for the connection's lifetime
//! (mirroring ADR-0147's "session is the identity boundary" stance, lifted to
//! the connection that will own one or more sessions). A credential that
//! doesn't resolve is refused at the HTTP upgrade (401) — the peer never
//! reaches the WS loop. With no [`ServeAuth`] configured, `serve` behaves
//! byte-for-byte as [ADR-0048] describes.
//!
//! entanglement ships one reference implementation,
//! [`StaticTokenAuthenticator`] (a token → user map from a YAML file), the
//! same trait-seam precedent `PermissionResolver`/`GrantStore`/
//! `UserProviderStore` set: a real deployment implements the trait against
//! its own identity store (a DB, OIDC introspection).
//!
//! [ADR-0174]: ../../../docs/adr/0174-authenticated-multi-user-wire-head.md
//! [ADR-0147]: ../../../docs/adr/0147-multi-user-mode-embedder-api.md
//! [ADR-0048]: ../../../docs/adr/0048-serve-head-local-trust-model.md

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::http::HeaderMap;
use entanglement_core::{Holly, OutEvent, UserId};
use tokio::sync::broadcast::error::RecvError;

use crate::multi_user::SessionUserRegistry;

/// Resolves a wire credential to a [`UserId`] — the pluggable seam an
/// embedder implements against its own identity store. `None` refuses the
/// connection (401 at upgrade). Called once per connection, never per frame
/// (ADR-0174 §1); a revoked credential already bound to a live connection
/// keeps working until that connection closes.
pub trait WireAuthenticator: Send + Sync {
    fn authenticate(&self, credential: &str) -> Option<UserId>;
}

/// Everything authenticated mode needs, attached to the router: the
/// credential resolver and the session→user registry the head populates as
/// it authors trusted `Spawn`s (ADR-0174 §2-3). An embedder passes its own
/// shared [`SessionUserRegistry`] so `PerUserPermissionResolver`/
/// `PerUserGrantStore` read the same mapping; the CLI constructs a fresh one.
#[derive(Clone)]
pub struct ServeAuth {
    pub authenticator: Arc<dyn WireAuthenticator>,
    pub registry: SessionUserRegistry,
}

/// Reference [`WireAuthenticator`]: a static token → user map, for tests and
/// small deployments (`skutter serve --auth-tokens <file>`).
pub struct StaticTokenAuthenticator {
    tokens: HashMap<String, UserId>,
}

impl StaticTokenAuthenticator {
    pub fn new(tokens: HashMap<String, UserId>) -> Self {
        Self { tokens }
    }

    /// Load the YAML token file (`tokens: {token: user_id}`). **Fail-closed**:
    /// an unreadable file, a parse error, or an *empty* map is a hard error —
    /// an auth misconfiguration must never fall open to unauthenticated.
    pub fn from_file(path: &Path) -> Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TokenFile {
            tokens: HashMap<String, String>,
        }
        warn_if_world_readable(path);
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading auth token file {}", path.display()))?;
        let parsed: TokenFile = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing auth token file {}", path.display()))?;
        if parsed.tokens.is_empty() {
            bail!(
                "auth token file {} has an empty `tokens:` map — refusing to start \
                 an authenticated head no one can connect to",
                path.display()
            );
        }
        Ok(Self::new(
            parsed
                .tokens
                .into_iter()
                .map(|(token, user)| (token, UserId::new(user)))
                .collect(),
        ))
    }
}

impl WireAuthenticator for StaticTokenAuthenticator {
    fn authenticate(&self, credential: &str) -> Option<UserId> {
        self.tokens.get(credential).cloned()
    }
}

/// The token file holds live credentials — mirror the managed-file courtesy
/// and warn (never fail) when it is group/world-readable.
fn warn_if_world_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{:o}", mode & 0o777),
                    "auth token file is group/world-readable — consider chmod 600"
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Strict `Authorization: Bearer <token>` parse — the only credential
/// location (ADR-0174's example shape). Case-insensitive scheme, exactly one
/// non-empty token, no fallback locations.
pub(super) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty() && !token.contains(' ')).then_some(token)
}

/// Fold the engine broadcast into the registry (ADR-0174 §3): the connection
/// handler registers a root synchronously at spawn-send, but a **spawned
/// child**'s inherited `user` only surfaces on its `SessionStarted` — this
/// task picks those up, and forgets every ended/hibernated session so the
/// map never grows for the process lifetime (hibernation-forget matches the
/// lazy-`Prompt` path's blank-respawn-with-fresh-`Spawn` root behavior).
pub(super) fn spawn_registry_maintainer(
    holly: &Holly,
    registry: SessionUserRegistry,
) -> tokio::task::JoinHandle<()> {
    let mut events = holly.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(OutEvent::SessionStarted {
                    session,
                    user: Some(user),
                    ..
                }) => {
                    registry.register(session, user);
                }
                Ok(OutEvent::SessionEnded { session, .. })
                | Ok(OutEvent::SessionHibernated { session, .. }) => {
                    registry.forget(&session);
                }
                Ok(_) => {}
                // A dropped frame under lag can only delay a child's
                // registration until nothing — the sync spawn-send path covers
                // roots, and a missed forget leaves a stale mark that the next
                // spawn-author overwrite corrects. Keep going, like the WS
                // relay (#158).
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("serve auth: registry maintainer lagged, skipped {n} events");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn static_tokens_resolve_and_unknown_refuses() {
        let auth = StaticTokenAuthenticator::new(
            [("tok-a".to_string(), UserId::new("alice"))]
                .into_iter()
                .collect(),
        );
        assert_eq!(auth.authenticate("tok-a"), Some(UserId::new("alice")));
        assert_eq!(auth.authenticate("tok-b"), None);
        assert_eq!(auth.authenticate(""), None);
    }

    #[test]
    fn bearer_token_parses_the_strict_shape_only() {
        assert_eq!(bearer_token(&headers("Bearer tok")), Some("tok"));
        // Case-insensitive scheme.
        assert_eq!(bearer_token(&headers("bearer tok")), Some("tok"));
        // Wrong scheme, no scheme, empty token, embedded space: all refused.
        assert_eq!(bearer_token(&headers("Basic dXNlcjpwdw==")), None);
        assert_eq!(bearer_token(&headers("tok")), None);
        assert_eq!(bearer_token(&headers("Bearer ")), None);
        assert_eq!(bearer_token(&headers("Bearer a b")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn token_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.yml");
        std::fs::write(&path, "tokens:\n  tok-a: alice\n  tok-b: bob\n").unwrap();
        let auth = StaticTokenAuthenticator::from_file(&path).unwrap();
        assert_eq!(auth.authenticate("tok-a"), Some(UserId::new("alice")));
        assert_eq!(auth.authenticate("tok-b"), Some(UserId::new("bob")));
    }

    #[test]
    fn token_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        assert!(StaticTokenAuthenticator::from_file(&dir.path().join("absent.yml")).is_err());
        // Malformed YAML.
        let bad = dir.path().join("bad.yml");
        std::fs::write(&bad, "tokens: [not, a, map\n").unwrap();
        assert!(StaticTokenAuthenticator::from_file(&bad).is_err());
        // Unknown top-level key (a typo'd `token:` must not silently yield an
        // open head).
        let typo = dir.path().join("typo.yml");
        std::fs::write(&typo, "token:\n  tok-a: alice\n").unwrap();
        assert!(StaticTokenAuthenticator::from_file(&typo).is_err());
        // Empty map.
        let empty = dir.path().join("empty.yml");
        std::fs::write(&empty, "tokens: {}\n").unwrap();
        assert!(StaticTokenAuthenticator::from_file(&empty).is_err());
    }
}
