//! Multi-user mode (#522, ADR-0147): per-user providers, API keys, RPM
//! budgets, permission ceiling, and grants in one running engine — the
//! **embedder library API** an in-process embedder wires instead of the
//! process-global defaults `skutter`'s CLI builds. `serve` stays single-user
//! (ADR-0048); an authenticated multi-user wire head is designed (not yet
//! built) by ADR-0174.
//!
//! - [`provider`] — per-user provider catalog + API keys, feeding
//!   [`EngineConfig::model_resolver`][entanglement_core::EngineConfig] (#522
//!   point 2). Keys never touch `std::env`.
//! - [`permission`] — per-user permission ceiling + "always allow" grants,
//!   built on the existing #311 [`PermissionResolver`][crate::policy::PermissionResolver]/
//!   [`GrantStore`][crate::policy::GrantStore] seams (#522 point 3).
//! - [`aux`] — per-user auxiliary-model (`summarize`/`session_title`/`narrate`)
//!   pins, feeding a per-user [`AuxLlmRegistry`][crate::aux_llm::AuxLlmRegistry]
//!   (#635, closing the gap ADR-0154's "Consequences" left open).
//!
//! Both sub-modules need to map a running session back to the user it
//! belongs to. Core carries `Session.user` internally but the policy/grant
//! traits only ever see a `&SessionId` (mirroring `ProfileResolver`'s own
//! session-keyed tracking) — so the embedder that calls `InMsg::Spawn { user:
//! Some(user), .. }` registers the same mapping here via
//! [`SessionUserRegistry`], the single directory both sub-modules read.

pub mod permission;
// Behind `provider`: builds `entanglement_provider::HttpClient`-backed `Llm`
// factories directly, exactly like the crate's `throttle` module.
#[cfg(feature = "provider")]
pub mod provider;
// Per-user auxiliary-model pins (#635, ADR-0154's deferred per-user aux pins,
// ledger row 15). Gated the same as `provider` above: it hands out
// `AuxLlmRegistry`s built from `entanglement_core::LlmFactory`/`ModelResolver`.
#[cfg(feature = "provider")]
pub mod aux;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::{SessionId, UserId};

/// Tracks which [`UserId`] a live session belongs to. The embedder populates
/// this itself — it already knows the mapping, having chosen `user` when it
/// sent the session's `InMsg::Spawn` — and [`permission::PerUserPermissionResolver`]/
/// [`permission::PerUserGrantStore`] consult it to scope a decision or grant
/// to the right user. Cheap to clone (an `Arc` inside); share one instance
/// across the resolver, the grant store, and the spawn call sites.
#[derive(Clone, Default)]
pub struct SessionUserRegistry {
    users: Arc<Mutex<HashMap<SessionId, UserId>>>,
}

impl SessionUserRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `session`'s owning user — call right after sending the
    /// `InMsg::Spawn` that created it (root or child; a child's `user` is
    /// whatever core resolved for it, i.e. inherited from its parent).
    pub fn register(&self, session: SessionId, user: UserId) {
        self.users
            .lock()
            .expect("session-user registry lock poisoned")
            .insert(session, user);
    }

    /// The user `session` belongs to, or `None` for a single-user-mode
    /// session never registered here.
    pub fn user_for(&self, session: &SessionId) -> Option<UserId> {
        self.users
            .lock()
            .expect("session-user registry lock poisoned")
            .get(session)
            .cloned()
    }

    /// Drop `session`'s entry — call on `SessionEnded`/`SessionHibernated`
    /// (or a `CloseSession` cascade) to keep the map from growing unbounded.
    pub fn forget(&self, session: &SessionId) {
        self.users
            .lock()
            .expect("session-user registry lock poisoned")
            .remove(session);
    }
}
