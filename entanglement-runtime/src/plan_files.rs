//! Per-session staleness tracking for `propose_plan`'s file-backed plans (#513,
//! ADR-0145).
//!
//! A plan is a file under `.entanglement/plans/`. The staleness guard requires
//! the agent to be the *last* party to have touched the file it resubmits via
//! `propose_plan(path=...)`: if the user edited it out of band (through an
//! editor, not through this session's own tool calls), a stale resubmit is
//! refused. Tracked as a session-scoped binding — the plan file's root-relative
//! path plus the content hash the agent last knew — kept fresh two ways:
//!
//! 1. `propose_plan` itself, every time it materializes (`content`) or reads
//!    (`path`) the file — see [`Self::record`].
//! 2. Passively, off the tool executor's own `OutEvent::FileChange` audit
//!    broadcast (#202, ADR-0060) whenever this session's own `edit`/`write`/
//!    `apply_patch` touches the bound path — see [`Self::note_file_change`] —
//!    so the intended review loop (the agent edits phase checkboxes directly,
//!    then re-`propose_plan`s the same file) never trips the guard, while a
//!    change the runtime never saw itself execute (the user's own edit) does.
//!
//! Content-addressed (a SHA-256 hash), not mtime-based: the hash is the
//! authoritative comparison at check time regardless, so tracking mtime
//! alongside it would add filesystem-resolution flakiness with no correctness
//! benefit — a deliberate simplification over the (mtime, hash) pair the
//! design sketch mentions (ADR-0145).

use std::collections::HashMap;
use std::sync::Mutex;

use entanglement_core::SessionId;

/// What a session last knew about its bound plan file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBinding {
    /// Root-relative path, e.g. `.entanglement/plans/<id>.md`.
    pub rel_path: String,
    /// Lowercase hex SHA-256 of the content as last known to the agent.
    pub hash: String,
}

/// Per-session plan-file bindings. Constructed once per tool-executor run and
/// shared (via `Arc`, held by the caller) between `propose_plan`'s dispatch arm
/// and the `FileChange`-listening background task.
#[derive(Default)]
pub struct PlanFileRegistry {
    bindings: Mutex<HashMap<SessionId, PlanBinding>>,
}

impl PlanFileRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session's current binding, if any.
    pub fn get(&self, session: &SessionId) -> Option<PlanBinding> {
        self.bindings.lock().unwrap().get(session).cloned()
    }

    /// Record that `session` now knows `rel_path` has content hashing to
    /// `hash` — called by `propose_plan` after every successful
    /// materialize/read. Rebinding to a different path simply replaces the
    /// prior binding (switching plan files resets tracking).
    pub fn record(&self, session: SessionId, rel_path: String, hash: String) {
        self.bindings
            .lock()
            .unwrap()
            .insert(session, PlanBinding { rel_path, hash });
    }

    /// Fold an `OutEvent::FileChange` the tool executor just broadcast: if
    /// `session` has a binding for `changed_path` (already root-relative,
    /// matching how [`Self::record`] stores it), refresh its hash — the change
    /// came from this session's own `edit`/`write`/`apply_patch`, so the agent
    /// still "knows" the file's true content. A change to any other path, or a
    /// session with no binding yet, is a no-op.
    pub fn note_file_change(&self, session: &SessionId, changed_path: &str, hash: &str) {
        let mut map = self.bindings.lock().unwrap();
        if let Some(binding) = map.get_mut(session) {
            if binding.rel_path == changed_path {
                binding.hash = hash.to_string();
            }
        }
    }

    /// Drop a session's bookkeeping when it ends.
    pub fn forget_session(&self, session: &SessionId) {
        self.bindings.lock().unwrap().remove(session);
    }

    /// Every current binding, for the plans-folder watcher (#627) to diff
    /// against what's actually on disk. A snapshot, not a live view — the
    /// watcher's debounced firing is infrequent enough that a clone is cheap
    /// and avoids holding the lock across its filesystem reads.
    pub fn snapshot(&self) -> Vec<(SessionId, PlanBinding)> {
        self.bindings
            .lock()
            .unwrap()
            .iter()
            .map(|(session, binding)| (session.clone(), binding.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_get_roundtrips() {
        let reg = PlanFileRegistry::new();
        let s = SessionId::new("s1");
        reg.record(s.clone(), ".entanglement/plans/s1.md".into(), "h1".into());
        let b = reg.get(&s).unwrap();
        assert_eq!(b.rel_path, ".entanglement/plans/s1.md");
        assert_eq!(b.hash, "h1");
    }

    #[test]
    fn get_on_unknown_session_is_none() {
        let reg = PlanFileRegistry::new();
        assert!(reg.get(&SessionId::new("nope")).is_none());
    }

    #[test]
    fn note_file_change_refreshes_a_matching_binding() {
        let reg = PlanFileRegistry::new();
        let s = SessionId::new("s1");
        reg.record(s.clone(), ".entanglement/plans/s1.md".into(), "h1".into());
        reg.note_file_change(&s, ".entanglement/plans/s1.md", "h2");
        assert_eq!(reg.get(&s).unwrap().hash, "h2");
    }

    #[test]
    fn note_file_change_ignores_a_different_path() {
        let reg = PlanFileRegistry::new();
        let s = SessionId::new("s1");
        reg.record(s.clone(), ".entanglement/plans/s1.md".into(), "h1".into());
        reg.note_file_change(&s, "src/main.rs", "h2");
        assert_eq!(reg.get(&s).unwrap().hash, "h1");
    }

    #[test]
    fn note_file_change_on_unbound_session_is_a_noop() {
        let reg = PlanFileRegistry::new();
        let s = SessionId::new("s1");
        reg.note_file_change(&s, ".entanglement/plans/s1.md", "h2");
        assert!(reg.get(&s).is_none());
    }

    #[test]
    fn forget_session_drops_the_binding() {
        let reg = PlanFileRegistry::new();
        let s = SessionId::new("s1");
        reg.record(s.clone(), ".entanglement/plans/s1.md".into(), "h1".into());
        reg.forget_session(&s);
        assert!(reg.get(&s).is_none());
    }

    #[test]
    fn snapshot_lists_every_binding() {
        let reg = PlanFileRegistry::new();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        reg.record(a.clone(), ".entanglement/plans/a.md".into(), "ha".into());
        reg.record(b.clone(), ".entanglement/plans/b.md".into(), "hb".into());
        let mut snap = reg.snapshot();
        snap.sort_by_key(|x| x.0.to_string());
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].0, a);
        assert_eq!(snap[0].1.rel_path, ".entanglement/plans/a.md");
        assert_eq!(snap[1].0, b);
    }

    #[test]
    fn snapshot_is_empty_for_a_fresh_registry() {
        let reg = PlanFileRegistry::new();
        assert!(reg.snapshot().is_empty());
    }
}
