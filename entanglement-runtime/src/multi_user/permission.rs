//! Per-user permission ceiling + "always allow" grants (#522 point 3,
//! ADR-0147), built on the #311 pluggable policy seams
//! ([`PermissionResolver`]/[`GrantStore`], `crate::policy`) rather than
//! forking `tool_runner`'s executor.
//!
//! The process-global equivalents — the config `permissions:` ceiling (#172)
//! and the managed `grants.yml` file ([`DefaultGrantStore`][crate::policy::DefaultGrantStore])
//! — apply to *every* session in the process alike. A multi-user engine needs
//! a second, per-user layer on top: [`PerUserPermissionResolver`] clamps an
//! inner resolver's grade by the resolving session's *own user's* ceiling
//! (composing with, never replacing, any process-global ceiling the inner
//! resolver already applies), and [`PerUserGrantStore`] scopes an `Always`
//! grant's storage key to the granting session's user, so it is consulted
//! only for that same user's later sessions.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use entanglement_core::{ApprovalScope, Permission, PermissionProfile, SessionId, UserId};

use crate::permission::{clamp_to_base, permission_arg, permission_workdir};
use crate::policy::{GrantStore, PermissionResolver};

use super::SessionUserRegistry;

/// Clamp an inner [`PermissionResolver`]'s grade by the resolving session's
/// own user's ceiling. `ceiling_for` is consulted on every call (mirroring
/// [`ProfileResolver`][crate::policy::ProfileResolver]'s own per-call
/// resolution) — an embedder backed by a DB should cache. A session with no
/// registered user (single-user-mode traffic sharing a multi-user engine)
/// passes `inner`'s result through unclamped, exactly as an untouched #172
/// config ceiling is a no-op.
pub struct PerUserPermissionResolver<R: PermissionResolver> {
    inner: R,
    session_users: SessionUserRegistry,
    ceiling_for: Box<dyn Fn(&UserId) -> PermissionProfile + Send + Sync>,
}

impl<R: PermissionResolver> PerUserPermissionResolver<R> {
    pub fn new(
        inner: R,
        session_users: SessionUserRegistry,
        ceiling_for: impl Fn(&UserId) -> PermissionProfile + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            session_users,
            ceiling_for: Box::new(ceiling_for),
        }
    }
}

#[async_trait]
impl<R: PermissionResolver> PermissionResolver for PerUserPermissionResolver<R> {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        let perm = self.inner.resolve(session, tool, input).await;
        let Some(user) = self.session_users.user_for(session) else {
            return perm;
        };
        let ceiling = (self.ceiling_for)(&user);
        let arg = permission_arg(tool, input);
        let workdir = permission_workdir(tool, input);
        clamp_to_base(perm, &ceiling, tool, arg.as_deref(), workdir.as_deref())
    }
}

/// `(tool, arg)` — the exact-match grant key, mirroring
/// [`FileGrantStore`][crate::grants::FileGrantStore]'s private `GrantKey`.
type GrantKey = (String, Option<String>);

/// Per-user "always allow" grants. `Session` scope is keyed by [`SessionId`]
/// exactly like [`DefaultGrantStore`][crate::policy::DefaultGrantStore] — a
/// session belongs to exactly one user already, so no extra isolation is
/// needed there. `Always` scope is keyed by [`UserId`] instead of being one
/// flat process-wide set: a grant recorded by one user's session is never
/// consulted for another user's, even for the identical `(tool, arg)` pair.
///
/// In-memory only, no managed file — the reference implementation for tests
/// and small deployments. A production multi-tenant embedder with its own
/// per-user database should implement [`GrantStore`] directly against it
/// instead (per the trait's own doc: `is_granted` can simply return `false`
/// there, with the durable check folded into a custom
/// `PermissionResolver::resolve`).
#[derive(Default)]
pub struct PerUserGrantStore {
    session_users: SessionUserRegistry,
    session_grants: Mutex<HashMap<SessionId, HashSet<GrantKey>>>,
    always_grants: Mutex<HashMap<UserId, HashSet<GrantKey>>>,
}

impl PerUserGrantStore {
    pub fn new(session_users: SessionUserRegistry) -> Self {
        Self {
            session_users,
            session_grants: Mutex::new(HashMap::new()),
            always_grants: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl GrantStore for PerUserGrantStore {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool {
        let key = (tool.to_string(), arg.map(str::to_string));
        let session_hit = self
            .session_grants
            .lock()
            .expect("per-user session grants lock poisoned")
            .get(session)
            .is_some_and(|set| set.contains(&key));
        if session_hit {
            return true;
        }
        let Some(user) = self.session_users.user_for(session) else {
            return false;
        };
        self.always_grants
            .lock()
            .expect("per-user always grants lock poisoned")
            .get(&user)
            .is_some_and(|set| set.contains(&key))
    }

    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
    ) {
        let key = (tool.to_string(), arg.map(str::to_string));
        match scope {
            ApprovalScope::Once => {}
            // `SessionDir` degrades to an exact `Session` grant: this store
            // doesn't override `grant_session_dir` (the trait default is a
            // no-op echo), so directory-covering was never activated — an
            // exact match here is the same safe degrade the trait doc
            // describes for a tool outside the read-only triad.
            ApprovalScope::Session | ApprovalScope::SessionDir => {
                self.session_grants
                    .lock()
                    .expect("per-user session grants lock poisoned")
                    .entry(session.clone())
                    .or_default()
                    .insert(key);
            }
            ApprovalScope::Always => {
                // No known user for this session (single-user-mode traffic on
                // a multi-user engine) has nowhere isolated to durably land —
                // dropping it (never prompting again is the unsafe direction)
                // is the fail-safe choice, mirroring how an unresolved session
                // fails closed elsewhere in the permission ladder.
                if let Some(user) = self.session_users.user_for(session) {
                    self.always_grants
                        .lock()
                        .expect("per-user always grants lock poisoned")
                        .entry(user)
                        .or_default()
                        .insert(key);
                }
            }
        }
    }

    fn forget_session(&self, session: &SessionId) {
        self.session_grants
            .lock()
            .expect("per-user session grants lock poisoned")
            .remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::Permission;

    struct AlwaysAsk;

    #[async_trait]
    impl PermissionResolver for AlwaysAsk {
        async fn resolve(&self, _session: &SessionId, _tool: &str, _input: &str) -> Permission {
            Permission::Ask
        }
    }

    fn ceiling(perm: Permission) -> PermissionProfile {
        PermissionProfile::new(perm)
    }

    #[tokio::test]
    async fn a_users_ceiling_clamps_their_own_session() {
        let session_users = SessionUserRegistry::new();
        let alice = UserId::new("alice");
        let session = SessionId::new("s1");
        session_users.register(session.clone(), alice.clone());

        let resolver = PerUserPermissionResolver::new(AlwaysAsk, session_users, move |u| {
            assert_eq!(*u, alice);
            ceiling(Permission::Deny)
        });

        assert_eq!(
            resolver.resolve(&session, "bash", "{}").await,
            Permission::Deny,
            "the user's own ceiling must tighten the inner Ask down to Deny"
        );
    }

    #[tokio::test]
    async fn a_session_with_no_registered_user_is_unaffected() {
        let session_users = SessionUserRegistry::new();
        let resolver =
            PerUserPermissionResolver::new(AlwaysAsk, session_users, |_| ceiling(Permission::Deny));
        let session = SessionId::new("anonymous");
        assert_eq!(
            resolver.resolve(&session, "bash", "{}").await,
            Permission::Ask,
            "no user registered means the per-user ceiling never applies"
        );
    }

    #[tokio::test]
    async fn always_grant_from_one_user_never_leaks_to_another() {
        let session_users = SessionUserRegistry::new();
        let alice = UserId::new("alice");
        let bob = UserId::new("bob");
        let alice_session = SessionId::new("alice-s1");
        let bob_session = SessionId::new("bob-s1");
        session_users.register(alice_session.clone(), alice);
        session_users.register(bob_session.clone(), bob);

        let store = PerUserGrantStore::new(session_users);
        store
            .record(&alice_session, "bash", Some("ls"), ApprovalScope::Always)
            .await;

        assert!(store.is_granted(&alice_session, "bash", Some("ls")));
        assert!(
            !store.is_granted(&bob_session, "bash", Some("ls")),
            "bob must never see alice's always-allow grant"
        );
    }

    #[tokio::test]
    async fn a_later_session_for_the_same_user_sees_the_always_grant() {
        let session_users = SessionUserRegistry::new();
        let alice = UserId::new("alice");
        let first = SessionId::new("alice-s1");
        let later = SessionId::new("alice-s2");
        session_users.register(first.clone(), alice.clone());
        session_users.register(later.clone(), alice);

        let store = PerUserGrantStore::new(session_users);
        store
            .record(&first, "bash", Some("ls"), ApprovalScope::Always)
            .await;

        assert!(
            store.is_granted(&later, "bash", Some("ls")),
            "an always grant persists across the same user's sessions"
        );
    }

    #[tokio::test]
    async fn session_scope_grant_does_not_cross_sessions() {
        let session_users = SessionUserRegistry::new();
        let alice = UserId::new("alice");
        let s1 = SessionId::new("alice-s1");
        let s2 = SessionId::new("alice-s2");
        session_users.register(s1.clone(), alice.clone());
        session_users.register(s2.clone(), alice);

        let store = PerUserGrantStore::new(session_users);
        store
            .record(&s1, "bash", Some("ls"), ApprovalScope::Session)
            .await;

        assert!(store.is_granted(&s1, "bash", Some("ls")));
        assert!(
            !store.is_granted(&s2, "bash", Some("ls")),
            "a Session-scope grant must not leak to a sibling session, even same user"
        );
    }

    #[tokio::test]
    async fn forget_session_clears_only_the_session_scope_grant() {
        let session_users = SessionUserRegistry::new();
        let alice = UserId::new("alice");
        let session = SessionId::new("s1");
        session_users.register(session.clone(), alice);

        let store = PerUserGrantStore::new(session_users);
        store
            .record(&session, "bash", Some("ls"), ApprovalScope::Session)
            .await;
        store
            .record(&session, "bash", Some("pwd"), ApprovalScope::Always)
            .await;
        store.forget_session(&session);

        assert!(!store.is_granted(&session, "bash", Some("ls")));
        assert!(
            store.is_granted(&session, "bash", Some("pwd")),
            "the durable always grant survives forget_session"
        );
    }
}
