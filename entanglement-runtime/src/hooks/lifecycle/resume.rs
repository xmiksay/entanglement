//! Which `SessionStarted`s a pending `InMsg::Resume` will cause, so the
//! lifecycle tracker can report `source: resume` for exactly those (#734).
//!
//! Tracked per `Resume`, not per id (#791): a duplicate `Resume` refused as
//! already-live must not spend the mark of the original one whose start is
//! still in flight.

use std::collections::{HashMap, HashSet};

use entanglement_core::{InMsg, OutEvent, SessionId};

/// One `Resume` still waiting on its starts: the target and every id it will
/// re-announce (the target included) that has not started yet.
#[derive(Debug)]
struct Pending {
    target: SessionId,
    ids: HashSet<SessionId>,
}

/// Pending resumes in arrival order.
#[derive(Debug, Default)]
pub(super) struct ResumeMarks(Vec<Pending>);

impl ResumeMarks {
    /// Note a `Resume` frame (anything else is ignored).
    ///
    /// Mirrors core's cascade rule (`supervisor/resume.rs`): only the target
    /// and the sessions of *its* spawn sub-tree still live as of the log's
    /// end — a `SessionStarted` with no later `SessionEnded`/
    /// `SessionHibernated` — are re-announced, so only those are marked.
    /// Marking the rest (the root's other branches, when a non-root target
    /// replays the root's whole log) would leak them until they end.
    pub(super) fn observe(&mut self, msg: &InMsg) {
        let InMsg::Resume { session, records } = msg else {
            return;
        };
        let mut live: Vec<&SessionId> = Vec::new();
        let mut parents: HashMap<&SessionId, &SessionId> = HashMap::new();
        for (_, ev) in records {
            match ev {
                OutEvent::SessionStarted {
                    session, parent, ..
                } => {
                    live.push(session);
                    if let Some(p) = parent {
                        parents.insert(session, p);
                    }
                }
                OutEvent::SessionEnded { session, .. }
                | OutEvent::SessionHibernated { session, .. } => live.retain(|s| *s != session),
                _ => {}
            }
        }
        let mut ids: HashSet<SessionId> = live
            .into_iter()
            .filter(|id| descends_from(id, session, &parents))
            .cloned()
            .collect();
        ids.insert(session.clone());
        self.0.push(Pending {
            target: session.clone(),
            ids,
        });
    }

    /// `session` started: was it a resume? Spends the *oldest* pending mark —
    /// core handles `Resume`s in order, so that is the one this start answers.
    pub(super) fn take(&mut self, session: &SessionId) -> bool {
        let Some(i) = self.0.iter().position(|r| r.ids.contains(session)) else {
            return false;
        };
        self.remove_id(i, session);
        true
    }

    /// The supervisor refused (or failed to replay) a resume of `session`: no
    /// start follows. The refusal answers the *latest* `Resume` still waiting
    /// on that id — an earlier one was already accepted, e.g. the original
    /// whose start races a refused duplicate — and a refused target
    /// re-announces nothing, so its whole entry goes.
    pub(super) fn refuse(&mut self, session: &SessionId) {
        let Some(i) = self.0.iter().rposition(|r| r.ids.contains(session)) else {
            return;
        };
        if self.0[i].target == *session {
            self.0.remove(i);
        } else {
            self.remove_id(i, session);
        }
    }

    /// `session` left memory: no pending resume can still start it.
    pub(super) fn forget(&mut self, session: &SessionId) {
        for r in &mut self.0 {
            r.ids.remove(session);
        }
        self.0.retain(|r| !r.ids.is_empty());
    }

    /// Every id some pending resume still marks.
    #[cfg(test)]
    pub(super) fn marked(&self) -> HashSet<&str> {
        self.0
            .iter()
            .flat_map(|r| r.ids.iter().map(|s| s.0.as_str()))
            .collect()
    }

    fn remove_id(&mut self, i: usize, session: &SessionId) {
        self.0[i].ids.remove(session);
        if self.0[i].ids.is_empty() {
            self.0.remove(i);
        }
    }
}

/// Whether `id` is `ancestor` or in its spawn sub-tree, per the log's parent
/// links. Bounded by the link count, so a malformed cyclic log can't spin.
fn descends_from(
    id: &SessionId,
    ancestor: &SessionId,
    parents: &HashMap<&SessionId, &SessionId>,
) -> bool {
    let mut cur = id;
    for _ in 0..=parents.len() {
        if cur == ancestor {
            return true;
        }
        match parents.get(cur) {
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}
