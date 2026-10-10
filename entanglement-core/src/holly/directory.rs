//! The live-session directory: one [`SessionInfo`] per session with a running
//! task, written only by the supervisor and read synchronously through
//! [`Holly::session_info`][super::Holly::session_info] (ADR-0211).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::protocol::{SessionId, SessionInfo};

/// Shared, cheaply-cloned handle. The supervisor inserts an entry before the
/// session task starts (so it is present by `SessionStarted`) and removes it
/// when it closes or hibernates the id. The mutex is never held across an
/// `.await`.
#[derive(Clone, Default)]
pub(crate) struct SessionDirectory(Arc<Mutex<HashMap<SessionId, SessionInfo>>>);

impl SessionDirectory {
    // A poisoned lock only means another thread panicked mid-write of a plain
    // map insert/remove; the map itself is still coherent, so keep serving it
    // rather than cascading the panic into every reader.
    fn map(&self) -> MutexGuard<'_, HashMap<SessionId, SessionInfo>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self, session: &SessionId) -> Option<SessionInfo> {
        self.map().get(session).cloned()
    }

    pub(crate) fn insert(&self, info: SessionInfo) {
        self.map().insert(info.session.clone(), info);
    }

    pub(crate) fn remove(&self, session: &SessionId) {
        self.map().remove(session);
    }

    /// Every entry, sorted by id — the `ListSessions` reply order.
    pub(crate) fn snapshot(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self.map().values().cloned().collect();
        list.sort_by(|a, b| a.session.0.cmp(&b.session.0));
        list
    }

    /// The ids of every root session (`root: true`).
    pub(crate) fn roots(&self) -> Vec<SessionId> {
        self.map()
            .values()
            .filter(|m| m.root)
            .map(|m| m.session.clone())
            .collect()
    }
}
