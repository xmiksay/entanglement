//! Per-user MCP OAuth credential storage — the provider-hosted universal
//! interface (ADR-0181/ADR-0184, #687).
//!
//! [`TokenStore`] is deliberately per-*server*: it is the shape every
//! per-connection consumer ([`StoredTokenSource`][super::StoredTokenSource],
//! [`check`][super::check], the auth flows) binds at connect time, and a
//! single-user process implements it once over one credential file. A
//! multi-user embedder instead implements [`UserTokenStore`] — the same
//! contract widened by a [`UserId`] — over its own per-tenant storage, and
//! bridges one user's slice back into the plain [`TokenStore`] shape with
//! [`user_scoped`]. Every existing consumer keeps working unchanged; the
//! *runtime* never stores per-user state itself (ADR-0181).
//!
//! A constant user degrades to exactly the single-user behavior: one
//! [`user_scoped`] view over one user is indistinguishable from a plain
//! process-global store.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use super::{StoredAuth, TokenStore};
use crate::llm::UserId;

/// Per-user persistence for [`StoredAuth`], keyed `(user, server)`. The
/// embedder implements this once over its own storage (a DB table, a secrets
/// manager); [`InMemoryUserTokenStore`] is the reference impl for tests and
/// small deployments. Synchronous for the same reason [`TokenStore`] is: the
/// backing store is small and local, and a sync trait spares implementors an
/// async-trait indirection.
pub trait UserTokenStore: Send + Sync {
    fn load(&self, user: &UserId, server: &str) -> Result<Option<StoredAuth>>;
    fn save(&self, user: &UserId, server: &str, auth: &StoredAuth) -> Result<()>;
    fn delete(&self, user: &UserId, server: &str) -> Result<()>;

    /// The servers `user` holds a stored credential for (for status listings).
    fn servers(&self, user: &UserId) -> Vec<String>;

    /// Cross-process critical section scoped to `(user, server)` — the
    /// per-user mirror of [`TokenStore::with_exclusive`], with the same
    /// default (plain load→save, correct for a store not shared across
    /// processes) and the same purpose: serializing a token *refresh*, not
    /// just the eventual write (#631).
    fn with_exclusive(
        &self,
        user: &UserId,
        server: &str,
        f: Box<dyn FnOnce(Option<StoredAuth>) -> Result<StoredAuth> + '_>,
    ) -> Result<StoredAuth> {
        let current = self.load(user, server)?;
        let updated = f(current)?;
        self.save(user, server, &updated)?;
        Ok(updated)
    }
}

/// Bind one user's slice of a [`UserTokenStore`] into the plain per-connection
/// [`TokenStore`] shape every existing auth consumer takes. This is the whole
/// multi-user bridge: a per-user MCP connection is built with
/// `user_scoped(store, user)` where a single-user one passes its file-backed
/// store, and nothing downstream can tell the difference.
pub fn user_scoped(store: Arc<dyn UserTokenStore>, user: UserId) -> Arc<dyn TokenStore> {
    Arc::new(UserScopedTokenStore { store, user })
}

struct UserScopedTokenStore {
    store: Arc<dyn UserTokenStore>,
    user: UserId,
}

impl TokenStore for UserScopedTokenStore {
    fn load(&self, server: &str) -> Result<Option<StoredAuth>> {
        self.store.load(&self.user, server)
    }

    fn save(&self, server: &str, auth: &StoredAuth) -> Result<()> {
        self.store.save(&self.user, server, auth)
    }

    fn delete(&self, server: &str) -> Result<()> {
        self.store.delete(&self.user, server)
    }

    fn with_exclusive(
        &self,
        server: &str,
        f: Box<dyn FnOnce(Option<StoredAuth>) -> Result<StoredAuth> + '_>,
    ) -> Result<StoredAuth> {
        self.store.with_exclusive(&self.user, server, f)
    }
}

/// In-memory [`UserTokenStore`] — the reference impl. An embedder with real
/// per-user storage should implement the trait directly rather than mirroring
/// credentials into this map.
#[derive(Clone, Default)]
pub struct InMemoryUserTokenStore {
    auths: Arc<RwLock<HashMap<(UserId, String), StoredAuth>>>,
}

impl InMemoryUserTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl UserTokenStore for InMemoryUserTokenStore {
    fn load(&self, user: &UserId, server: &str) -> Result<Option<StoredAuth>> {
        Ok(self
            .auths
            .read()
            .expect("user token store lock poisoned")
            .get(&(user.clone(), server.to_string()))
            .cloned())
    }

    fn save(&self, user: &UserId, server: &str, auth: &StoredAuth) -> Result<()> {
        self.auths
            .write()
            .expect("user token store lock poisoned")
            .insert((user.clone(), server.to_string()), auth.clone());
        Ok(())
    }

    fn delete(&self, user: &UserId, server: &str) -> Result<()> {
        self.auths
            .write()
            .expect("user token store lock poisoned")
            .remove(&(user.clone(), server.to_string()));
        Ok(())
    }

    fn servers(&self, user: &UserId) -> Vec<String> {
        let mut names: Vec<String> = self
            .auths
            .read()
            .expect("user token store lock poisoned")
            .keys()
            .filter(|(u, _)| u == user)
            .map(|(_, s)| s.clone())
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::super::TokenSet;
    use super::*;

    fn auth(token: &str) -> StoredAuth {
        StoredAuth {
            client_id: "client".into(),
            client_secret: None,
            token_endpoint: "https://as.example/token".into(),
            revocation_endpoint: None,
            resource: None,
            tokens: TokenSet {
                access_token: token.into(),
                refresh_token: None,
                token_type: "Bearer".into(),
                expires_at: None,
                scope: None,
            },
        }
    }

    #[test]
    fn two_users_credentials_are_isolated() {
        let store = InMemoryUserTokenStore::new();
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");
        store.save(&alice, "linear", &auth("alice-token")).unwrap();

        let loaded = store.load(&alice, "linear").unwrap().unwrap();
        assert_eq!(loaded.tokens.access_token, "alice-token");
        assert!(store.load(&bob, "linear").unwrap().is_none());
        assert_eq!(store.servers(&alice), vec!["linear".to_string()]);
        assert!(store.servers(&bob).is_empty());

        store.delete(&alice, "linear").unwrap();
        assert!(store.load(&alice, "linear").unwrap().is_none());
    }

    #[test]
    fn user_scoped_view_is_a_plain_token_store_over_one_users_slice() {
        let store = Arc::new(InMemoryUserTokenStore::new());
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");
        let alice_view = user_scoped(store.clone(), alice.clone());
        let bob_view = user_scoped(store.clone(), bob.clone());

        alice_view.save("linear", &auth("alice-token")).unwrap();
        assert_eq!(
            alice_view
                .load("linear")
                .unwrap()
                .unwrap()
                .tokens
                .access_token,
            "alice-token"
        );
        assert!(bob_view.load("linear").unwrap().is_none());

        // The underlying user-keyed store sees the same credential.
        assert_eq!(
            store
                .load(&alice, "linear")
                .unwrap()
                .unwrap()
                .tokens
                .access_token,
            "alice-token"
        );
    }

    #[test]
    fn with_exclusive_default_chains_load_and_save_per_user() {
        let store = InMemoryUserTokenStore::new();
        let alice = UserId::new("alice");
        store.save(&alice, "linear", &auth("stale")).unwrap();

        let updated = store
            .with_exclusive(
                &alice,
                "linear",
                Box::new(|current| {
                    assert_eq!(current.unwrap().tokens.access_token, "stale");
                    Ok(auth("fresh"))
                }),
            )
            .unwrap();
        assert_eq!(updated.tokens.access_token, "fresh");
        assert_eq!(
            store
                .load(&alice, "linear")
                .unwrap()
                .unwrap()
                .tokens
                .access_token,
            "fresh"
        );
    }
}
