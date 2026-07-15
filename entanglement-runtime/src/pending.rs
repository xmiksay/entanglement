//! Lag-proof pending-decision registry (#156).
//!
//! A parked tool round-trip — a permission `Ask`, `ask_user`, `propose_plan`, or
//! a `rhai` binding approval — awaits the head's `Approve`/`Reject`/
//! `AnswerQuestion`. Each orchestrator used to hold its *own* `broadcast`
//! subscription of the engine's inbound fan-out and filter it to its
//! `(session, request_id)`. Under burst load (many sessions, > the inbound
//! channel capacity of frames between subscribe and answer) that per-task
//! subscriber could **lag** and silently drop the very decision it waited for —
//! the request then parked forever while the user believed they had answered.
//!
//! This registry moves decision delivery off the lossy per-task broadcast onto a
//! single `oneshot` per request. A parked task [`register`s](PendingDecisions::register)
//! its `(session, request_id)` *before* emitting its request event and awaits the
//! returned receiver; one dedicated router task (the executor's inbound watcher
//! in [`crate::tool_runner`]) is the sole inbound consumer and
//! [`resolve`s](PendingDecisions::resolve) each decision to its waiter. Because
//! the router does nothing per frame but a map lookup + `oneshot` send, it drains
//! far faster than a park loop that also competes with tool execution — so the
//! realistic lag window that dropped decisions is closed. A `Stop` for the
//! session unwinds every one of its waiters
//! ([`stop_session`](PendingDecisions::stop_session)).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use entanglement_core::SessionId;
use tokio::sync::oneshot;

use crate::seam::Decision;

type Waiters = HashMap<(SessionId, String), oneshot::Sender<Decision>>;

/// Shared map of parked tool round-trips awaiting a head decision. Cheap to
/// clone (an `Arc`); the executor hands a clone to its inbound router and to each
/// dispatch task. The `Mutex` is never held across an `.await`.
#[derive(Clone, Default)]
pub struct PendingDecisions {
    inner: Arc<Mutex<Waiters>>,
}

impl PendingDecisions {
    /// Register a waiter for `(session, request_id)` and return the receiver it
    /// awaits. Call this **before** emitting the request event so the router can
    /// never process the decision before the waiter exists (register-then-emit
    /// closes the race the pre-#156 "subscribe before handing off" discipline
    /// guarded). A second register for the same key drops the earlier waiter's
    /// sender, so that stale park resolves to [`Decision::Stop`] via
    /// [`await_decision`] — the desired outcome for a re-offered request.
    pub fn register(&self, session: &SessionId, request_id: &str) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        self.lock()
            .insert((session.clone(), request_id.to_string()), tx);
        rx
    }

    /// Deliver `decision` to the waiter for `(session, request_id)`, if one is
    /// still parked. A no-op when none is (already resolved, or a decision for an
    /// unknown/duplicate request). A dropped receiver (the waiter already unwound)
    /// is likewise harmless.
    pub fn resolve(&self, session: &SessionId, request_id: &str, decision: Decision) {
        if let Some(tx) = self
            .lock()
            .remove(&(session.clone(), request_id.to_string()))
        {
            let _ = tx.send(decision);
        }
    }

    /// Unwind every waiter for `session` with [`Decision::Stop`] (#167): a `Stop`
    /// is session-scoped, so it cancels all of the session's parked approvals at
    /// once. Core cancels the turn on the same `Stop`, so no `ToolResult` is owed.
    pub fn stop_session(&self, session: &SessionId) {
        let mut guard = self.lock();
        let keys: Vec<_> = guard
            .keys()
            .filter(|(s, _)| s == session)
            .cloned()
            .collect();
        for key in keys {
            if let Some(tx) = guard.remove(&key) {
                let _ = tx.send(Decision::Stop);
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Waiters> {
        self.inner.lock().expect("pending decisions mutex poisoned")
    }
}

/// Await a registered waiter's decision. A dropped sender — the router resolved a
/// `Stop`, a re-register superseded this waiter, or the whole executor stopped —
/// resolves to [`Decision::Stop`], preserving the "inbox closed ⇒ unwind
/// silently" semantics the broadcast park had (ADR-0017).
pub async fn await_decision(rx: oneshot::Receiver<Decision>) -> Decision {
    rx.await.unwrap_or(Decision::Stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::ApprovalScope;

    #[tokio::test]
    async fn resolve_delivers_to_the_matching_waiter() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let rx = pending.register(&s, "req-1");
        pending.resolve(
            &s,
            "req-1",
            Decision::Approve {
                scope: ApprovalScope::Once,
            },
        );
        assert!(matches!(await_decision(rx).await, Decision::Approve { .. }));
    }

    #[tokio::test]
    async fn resolve_for_unknown_request_is_a_noop() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        // No waiter registered for req-x: resolving must not panic and must not
        // leak into a later same-key waiter.
        pending.resolve(&s, "req-x", Decision::Reject { reason: None });
        let rx = pending.register(&s, "req-x");
        pending.stop_session(&s);
        assert!(matches!(await_decision(rx).await, Decision::Stop));
    }

    #[tokio::test]
    async fn dropped_registry_unwinds_the_waiter_to_stop() {
        let s = SessionId::new("s");
        let rx = {
            let pending = PendingDecisions::default();
            pending.register(&s, "req-1")
            // `pending` (holding the sender) drops here.
        };
        assert!(matches!(await_decision(rx).await, Decision::Stop));
    }

    #[tokio::test]
    async fn stop_session_unwinds_only_its_own_waiters() {
        let pending = PendingDecisions::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let rx_a = pending.register(&a, "req");
        let rx_b = pending.register(&b, "req");
        pending.stop_session(&a);
        assert!(matches!(await_decision(rx_a).await, Decision::Stop));
        // b's waiter is untouched; resolving it still works.
        pending.resolve(&b, "req", Decision::Answer { answer: "x".into() });
        assert!(matches!(
            await_decision(rx_b).await,
            Decision::Answer { .. }
        ));
    }
}
