//! Session-lifecycle bookkeeping for [`super::AvailableMcp`] (#630): the
//! child→parent map [`ancestor_enabled`] walks to resolve `spec_visible` for
//! a spawned child, and the `SessionEnded` cleanup that keeps both it and the
//! enablement map from growing for the process lifetime. Split out of
//! `available.rs` along the 400-line file cap (mirrors `available_enable.rs`'s
//! #556 split) — sibling `#[path]` child module, so private fields stay
//! reachable via the normal descendant-module visibility rule.

use std::collections::HashSet;

use entanglement_core::SessionId;

use super::AvailableMcp;

/// Record `session`'s parent (`None` for a root) from its `SessionStarted`
/// event — feeds [`ancestor_enabled`]. A head-driven resume re-emits
/// `SessionStarted` for a previously-hibernated id; re-recording the same
/// link is harmless.
pub fn record_parent(avail: &AvailableMcp, session: SessionId, parent: Option<SessionId>) {
    avail
        .parents
        .lock()
        .expect("available-server parent mutex poisoned")
        .insert(session, parent);
}

/// Drop every trace of `session` on `SessionEnded`: its parent link and its
/// enablement mark on every server it had enabled — otherwise `enabled`/
/// `parents` grow for the process lifetime, one entry per session that ever
/// spawned or enabled a lazy server, never reclaimed. Deliberately **not**
/// called on `SessionHibernated`: a hibernated session is resumable under the
/// same id, and unlike the #539 tool overlay (whose runtime-side mirror a
/// resume rebuilds from core's replayed `ToolOverlayChanged` log), a lazy MCP
/// enable is never logged for replay — clearing it here would strand a
/// resumed session with no way to get its enabled tools back except
/// re-enabling by hand.
pub fn forget_session(avail: &AvailableMcp, session: &SessionId) {
    avail
        .parents
        .lock()
        .expect("available-server parent mutex poisoned")
        .remove(session);
    avail
        .enabled
        .lock()
        .expect("available-server enablement mutex poisoned")
        .retain(|_, sessions| {
            sessions.remove(session);
            !sessions.is_empty()
        });
}

/// Whether `session` inherits `sessions`' enablement through an ancestor
/// (#630): walks the parent chain [`record_parent`] fed, cycle-guarded
/// exactly like `permission::ancestor_chain` walks `SpawnGuard`'s own copy of
/// the same links. Resolved live at call time, not snapshotted at spawn — an
/// ancestor enabling *after* the child already exists is picked up too.
pub(super) fn ancestor_enabled(
    avail: &AvailableMcp,
    sessions: &HashSet<SessionId>,
    session: &SessionId,
) -> bool {
    let parents = avail
        .parents
        .lock()
        .expect("available-server parent mutex poisoned");
    let mut visited = HashSet::new();
    visited.insert(session.clone());
    let mut current = session.clone();
    while let Some(Some(parent)) = parents.get(&current) {
        if !visited.insert(parent.clone()) {
            break;
        }
        if sessions.contains(parent) {
            return true;
        }
        current = parent.clone();
    }
    false
}

#[cfg(test)]
#[path = "available_lifecycle_tests.rs"]
mod tests;
