//! The live-session directory: one [`SessionInfo`] per session with a running
//! task, plus the compaction-successor chain, written only by the supervisor
//! and read synchronously through [`Holly::session_info`][super::Holly::session_info]
//! and [`Holly::successor_of`][super::Holly::successor_of] (ADR-0211, ADR-0234).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::protocol::{SessionId, SessionInfo};

#[derive(Default)]
struct Inner {
    live: HashMap<SessionId, SessionInfo>,
    // source → successor (ADR-0205). Never pruned: a retired id stays
    // resolvable for as long as anything may still name it — a late tool
    // result, a sub-agent handle (ADR-0234). One entry per compaction.
    successors: HashMap<SessionId, SessionId>,
}

/// Shared, cheaply-cloned handle. The supervisor inserts an entry before the
/// session task starts (so it is present by `SessionStarted`) and removes it
/// when it closes or hibernates the id. The mutex is never held across an
/// `.await`.
#[derive(Clone, Default)]
pub(crate) struct SessionDirectory(Arc<Mutex<Inner>>);

impl SessionDirectory {
    // A poisoned lock only means another thread panicked mid-write of a plain
    // map insert/remove; the map itself is still coherent, so keep serving it
    // rather than cascading the panic into every reader.
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self, session: &SessionId) -> Option<SessionInfo> {
        self.inner().live.get(session).cloned()
    }

    pub(crate) fn insert(&self, info: SessionInfo) {
        self.inner().live.insert(info.session.clone(), info);
    }

    pub(crate) fn remove(&self, session: &SessionId) {
        self.inner().live.remove(session);
    }

    /// Re-link a live session under `parent` (ADR-0234). A no-op for an id
    /// with no entry.
    pub(crate) fn set_parent(&self, session: &SessionId, parent: &SessionId) {
        if let Some(info) = self.inner().live.get_mut(session) {
            info.parent = Some(parent.clone());
            info.root = false;
        }
    }

    /// Record that `successor` took over from `source` (ADR-0205).
    pub(crate) fn record_successor(&self, source: &SessionId, successor: &SessionId) {
        self.inner()
            .successors
            .insert(source.clone(), successor.clone());
    }

    /// Follow the compaction-successor chain from `session` to its last link.
    /// A long-running session can compact repeatedly, so a result dispatched
    /// before an early fork must walk the whole chain, not one link. Bounded
    /// by the map size so a cycle (which the single-use id rule already
    /// prevents) can't spin. `None` when `session` never compacted.
    pub(crate) fn successor_of(&self, session: &SessionId) -> Option<SessionId> {
        let inner = self.inner();
        let mut current = inner.successors.get(session)?.clone();
        for _ in 0..inner.successors.len() {
            match inner.successors.get(&current) {
                Some(next) => current = next.clone(),
                None => break,
            }
        }
        Some(current)
    }

    /// Every session `session` succeeded, nearest first: the compaction
    /// chain walked backwards. Each link has one source (an id is
    /// single-use), so the walk is a list. Bounded by the map size and
    /// cycle-safe (a visited set), so a corrupt map can't spin it. Empty
    /// when `session` never succeeded one.
    pub(crate) fn predecessors_of(&self, session: &SessionId) -> Vec<SessionId> {
        let inner = self.inner();
        let mut chain = Vec::new();
        let mut seen = HashSet::from([session.clone()]);
        let mut current = session;
        for _ in 0..inner.successors.len() {
            match inner.successors.iter().find(|(_, s)| *s == current) {
                Some((source, _)) if seen.insert(source.clone()) => {
                    chain.push(source.clone());
                    current = source;
                }
                _ => break,
            }
        }
        chain
    }

    /// The first link of `session`'s compaction chain, `session` itself
    /// when it never succeeded one.
    pub(crate) fn lineage_head(&self, session: &SessionId) -> SessionId {
        self.predecessors_of(session)
            .pop()
            .unwrap_or_else(|| session.clone())
    }

    /// Every entry, sorted by id — the `ListSessions` reply order.
    pub(crate) fn snapshot(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self.inner().live.values().cloned().collect();
        list.sort_by(|a, b| a.session.0.cmp(&b.session.0));
        list
    }

    /// The ids of every root session (`root: true`).
    pub(crate) fn roots(&self) -> Vec<SessionId> {
        self.inner()
            .live
            .values()
            .filter(|m| m.root)
            .map(|m| m.session.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_successor_chain_resolves_to_its_last_link() {
        let dir = SessionDirectory::default();
        let (a, b, c) = (
            SessionId::new("a"),
            SessionId::new("b"),
            SessionId::new("c"),
        );
        assert_eq!(dir.successor_of(&a), None);
        dir.record_successor(&a, &b);
        dir.record_successor(&b, &c);
        assert_eq!(dir.successor_of(&a), Some(c.clone()));
        assert_eq!(dir.successor_of(&b), Some(c.clone()));
        assert_eq!(dir.successor_of(&c), None);
        assert_eq!(dir.lineage_head(&c), a);
        assert_eq!(dir.lineage_head(&b), a);
        assert_eq!(dir.lineage_head(&a), a);
        assert_eq!(dir.predecessors_of(&c), [b.clone(), a.clone()]);
        assert_eq!(dir.predecessors_of(&b), std::slice::from_ref(&a));
        assert!(dir.predecessors_of(&a).is_empty());
    }

    /// A cycle (which the single-use id rule prevents) ends the backward
    /// walk instead of spinning it, and never names the start.
    #[test]
    fn the_backward_walk_survives_a_cycle() {
        let dir = SessionDirectory::default();
        let (a, b, c) = (
            SessionId::new("a"),
            SessionId::new("b"),
            SessionId::new("c"),
        );
        dir.record_successor(&a, &b);
        dir.record_successor(&b, &c);
        dir.record_successor(&c, &a);
        assert_eq!(dir.predecessors_of(&c), [b.clone(), a.clone()]);
        assert_eq!(dir.predecessors_of(&a), [c, b.clone()]);
        assert_eq!(dir.lineage_head(&a), b);
    }

    /// The walk is bounded by the map: a long chain is walked to its head,
    /// one step per recorded compaction.
    #[test]
    fn the_backward_walk_reaches_the_head_of_a_long_chain() {
        let dir = SessionDirectory::default();
        let ids: Vec<SessionId> = (0..50).map(|i| SessionId::new(format!("s{i}"))).collect();
        for pair in ids.windows(2) {
            dir.record_successor(&pair[0], &pair[1]);
        }
        let back = dir.predecessors_of(&ids[49]);
        assert_eq!(back.len(), 49);
        assert_eq!(back.first(), Some(&ids[48]));
        assert_eq!(back.last(), Some(&ids[0]));
    }

    #[test]
    fn set_parent_relinks_a_live_entry_only() {
        let dir = SessionDirectory::default();
        let (child, old, new) = (
            SessionId::new("child"),
            SessionId::new("old"),
            SessionId::new("new"),
        );
        dir.insert(SessionInfo {
            session: child.clone(),
            parent: Some(old),
            predecessor: None,
            agent: "general".into(),
            root: false,
            user: None,
        });
        dir.set_parent(&child, &new);
        assert_eq!(dir.get(&child).and_then(|i| i.parent), Some(new.clone()));
        dir.set_parent(&SessionId::new("ghost"), &new);
        assert!(dir.get(&SessionId::new("ghost")).is_none());
    }
}
