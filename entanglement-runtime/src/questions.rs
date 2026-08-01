//! Snapshot registry of open `ask_user` questions (#515).
//!
//! [`crate::pending::PendingDecisions`] tracks *who* is waiting for a
//! decision (a bare `oneshot::Sender`) but not *what* was asked — fine for its
//! generic use (permission `Ask`, `propose_plan`, `rhai` bindings), but
//! [`InMsg::ListQuestions`][entanglement_core::InMsg::ListQuestions] needs the
//! actual question content to answer a snapshot query. This registry is
//! `ask_user`-specific: [`crate::ask_user::run_ask_user`] inserts before
//! emitting each [`OutEvent::UserQuestion`][entanglement_core::OutEvent::UserQuestion]
//! and removes on every terminal outcome (answered, retracted, or the turn
//! stopped), so it always mirrors the set of calls still genuinely parked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use entanglement_core::{PendingQuestion, Question, Questions, SessionId};

type Open = HashMap<(SessionId, String), Vec<Question>>;

/// Shared map of open `ask_user` calls, keyed by `(session, request_id)`.
/// Cheap to clone (an `Arc`); the tool executor hands a clone to each
/// `ask_user` dispatch and to the `ListQuestions` responder.
#[derive(Clone, Default)]
pub struct OpenQuestions {
    inner: Arc<Mutex<Open>>,
}

impl OpenQuestions {
    /// Record `(session, request_id)` as open with `questions`, overwriting
    /// any prior content for the same key (a [`ReplaceQuestion`][entanglement_core::InMsg::ReplaceQuestion]
    /// re-insert).
    pub fn insert(&self, session: &SessionId, request_id: &str, questions: Vec<Question>) {
        self.lock()
            .insert((session.clone(), request_id.to_string()), questions);
    }

    /// Clear `(session, request_id)` — the call reached a terminal outcome.
    /// A no-op for an unknown key.
    pub fn remove(&self, session: &SessionId, request_id: &str) {
        self.lock()
            .remove(&(session.clone(), request_id.to_string()));
    }

    /// Snapshot every open question, optionally filtered to one `session`.
    /// Sorted by `(session, request_id)` for a deterministic wire reply.
    pub fn snapshot(&self, session: Option<&SessionId>) -> Vec<PendingQuestion> {
        let guard = self.lock();
        let mut list: Vec<PendingQuestion> = guard
            .iter()
            .filter(|((s, _), _)| session.is_none_or(|f| f == s))
            .map(|((s, rid), qs)| PendingQuestion {
                session: s.clone(),
                request_id: rid.clone(),
                questions: Questions(qs.clone()),
            })
            .collect();
        list.sort_by(|a, b| {
            (a.session.0.as_str(), a.request_id.as_str())
                .cmp(&(b.session.0.as_str(), b.request_id.as_str()))
        });
        list
    }

    fn lock(&self) -> MutexGuard<'_, Open> {
        self.inner.lock().expect("open questions mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> Question {
        Question {
            question: text.into(),
            options: Vec::new(),
            multi_select: false,
        }
    }

    #[test]
    fn snapshot_is_empty_with_nothing_open() {
        let open = OpenQuestions::default();
        assert!(open.snapshot(None).is_empty());
        assert!(open.snapshot(Some(&SessionId::new("s1"))).is_empty());
    }

    #[test]
    fn snapshot_returns_one_open_question() {
        let open = OpenQuestions::default();
        let s = SessionId::new("s1");
        open.insert(&s, "q1", vec![q("Which DB?")]);
        let list = open.snapshot(None);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session, s);
        assert_eq!(list[0].request_id, "q1");
        assert_eq!(list[0].questions.0[0].question, "Which DB?");
    }

    #[test]
    fn snapshot_spans_or_filters_multiple_sessions() {
        let open = OpenQuestions::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        open.insert(&a, "q1", vec![q("A?")]);
        open.insert(&b, "q1", vec![q("B?")]);

        let all = open.snapshot(None);
        assert_eq!(all.len(), 2);

        let only_a = open.snapshot(Some(&a));
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].session, a);
    }

    #[test]
    fn remove_clears_the_entry() {
        let open = OpenQuestions::default();
        let s = SessionId::new("s1");
        open.insert(&s, "q1", vec![q("Which DB?")]);
        open.remove(&s, "q1");
        assert!(open.snapshot(None).is_empty());
    }

    #[test]
    fn insert_overwrites_the_same_key() {
        let open = OpenQuestions::default();
        let s = SessionId::new("s1");
        open.insert(&s, "q1", vec![q("Original?")]);
        open.insert(&s, "q1", vec![q("Revised?")]);
        let list = open.snapshot(None);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].questions.0[0].question, "Revised?");
    }
}
