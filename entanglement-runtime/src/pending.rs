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
use std::time::Duration;

use entanglement_core::{ApprovalKind, PendingApproval as WireApproval, SessionId};
use tokio::sync::oneshot;

use crate::seam::Decision;

mod park;
pub use park::{approval, list_pending, park_approval, scopes_for};

/// What a parked decision is waiting on (#560 P12, ADR-0207 §12): carried
/// alongside the waiter so a listing (`InMsg::ListPending`) can describe a
/// parked approval without a second, drifting registry. `kind` is one of
/// `"tool"` (a permission `Ask`), `"plan"` (`propose_plan`), `"mode"`
/// (`request_mode`), `"rhai"` (a script binding's own `Ask`), or `"ask_user"`
/// (an `ask_user` call — carried here too for completeness, though a
/// `pending` listing folds those into its own open-questions section instead
/// of double-reporting them as approvals). `detail` is a short, already
/// human-readable label — the tool name for `"tool"`/`"rhai"`, the requested
/// mode for `"mode"` — never the full call input. `approval` is the full
/// `InMsg::ListPending` payload (#726) for every kind but `"ask_user"`.
#[derive(Clone)]
pub struct Parked {
    pub kind: &'static str,
    pub detail: String,
    pub approval: Option<WireApproval>,
}

type Waiters = HashMap<(SessionId, String), (oneshot::Sender<Decision>, Parked)>;

/// One parked decision, as a pending listing reports it. Sorted by `(session, request_id)` for a deterministic reply,
/// mirroring [`crate::questions::OpenQuestions::snapshot`].
pub struct PendingApproval {
    pub session: SessionId,
    pub request_id: String,
    pub kind: &'static str,
    pub detail: String,
}

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
    /// `kind`/`detail` describe what this park is waiting on (see [`Parked`]).
    pub fn register(
        &self,
        session: &SessionId,
        request_id: &str,
        kind: &'static str,
        detail: impl Into<String>,
    ) -> oneshot::Receiver<Decision> {
        self.insert(session, request_id, kind, detail.into(), None)
    }

    /// [`register`](Self::register) for an approval park, keeping its full
    /// [`WireApproval`] for `InMsg::ListPending` (#726). Prefer
    /// [`park_approval`], which also emits the matching `ToolRequest`.
    pub fn register_approval(
        &self,
        approval: WireApproval,
        detail: impl Into<String>,
    ) -> oneshot::Receiver<Decision> {
        let kind = match approval.kind {
            ApprovalKind::Tool => "tool",
            ApprovalKind::Plan => "plan",
            ApprovalKind::Mode => "mode",
            ApprovalKind::Rhai => "rhai",
        };
        let (session, request_id) = (approval.session.clone(), approval.request_id.clone());
        self.insert(&session, &request_id, kind, detail.into(), Some(approval))
    }

    fn insert(
        &self,
        session: &SessionId,
        request_id: &str,
        kind: &'static str,
        detail: String,
        approval: Option<WireApproval>,
    ) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        let parked = Parked {
            kind,
            detail,
            approval,
        };
        self.lock()
            .insert((session.clone(), request_id.to_string()), (tx, parked));
        rx
    }

    /// Every live parked approval whose session `in_scope` accepts, sorted by
    /// `(session, request_id)` (#726). Skips `ask_user` parks (those are
    /// `ListQuestions`' business) and any waiter that already gave up.
    pub fn approvals(&self, in_scope: impl Fn(&SessionId) -> bool) -> Vec<WireApproval> {
        let guard = self.lock();
        let mut list: Vec<WireApproval> = guard
            .iter()
            .filter(|((s, _), (tx, _))| !tx.is_closed() && in_scope(s))
            .filter_map(|(_, (_, parked))| parked.approval.clone())
            .collect();
        list.sort_by(|a, b| {
            (a.session.0.as_str(), a.request_id.as_str())
                .cmp(&(b.session.0.as_str(), b.request_id.as_str()))
        });
        list
    }

    /// Snapshot every parked decision, optionally filtered to one `session`.
    /// `None` spans every session, for a caller filtering to a whole spawn
    /// sub-tree locally rather than calling this once per member session.
    pub fn snapshot(&self, session: Option<&SessionId>) -> Vec<PendingApproval> {
        let guard = self.lock();
        let mut list: Vec<PendingApproval> = guard
            .iter()
            // A closed sender means the waiter is gone. A timeout removes its
            // own entry (`await_timed`); this filter remains for an aborted
            // task, whose entry nothing removes until `Stop`.
            .filter(|((s, _), (tx, _))| !tx.is_closed() && session.is_none_or(|f| f == s))
            .map(|((s, rid), (_, parked))| PendingApproval {
                session: s.clone(),
                request_id: rid.clone(),
                kind: parked.kind,
                detail: parked.detail.clone(),
            })
            .collect();
        list.sort_by(|a, b| {
            (a.session.0.as_str(), a.request_id.as_str())
                .cmp(&(b.session.0.as_str(), b.request_id.as_str()))
        });
        list
    }

    /// Deliver `decision` to the waiter for `(session, request_id)`, if one is
    /// still parked. A no-op when none is (already resolved, or a decision for an
    /// unknown/duplicate request). A dropped receiver (the waiter already unwound)
    /// is likewise harmless.
    pub fn resolve(&self, session: &SessionId, request_id: &str, decision: Decision) {
        if let Some((tx, _)) = self
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
            if let Some((tx, _)) = guard.remove(&key) {
                let _ = tx.send(Decision::Stop);
            }
        }
    }

    /// Drop the entry for `(session, request_id)` without delivering anything.
    pub fn forget(&self, session: &SessionId, request_id: &str) {
        self.lock()
            .remove(&(session.clone(), request_id.to_string()));
    }

    /// [`await_decision_timed`], removing the entry when the deadline elapses
    /// (#726): nothing else ever would, and in an unattended mode expiry is
    /// the normal path, so the map (each entry holding a full call `input`)
    /// would otherwise grow until the session stops.
    pub async fn await_timed(
        &self,
        session: &SessionId,
        request_id: &str,
        rx: oneshot::Receiver<Decision>,
        timeout: Option<Duration>,
    ) -> Option<Decision> {
        let decision = await_decision_timed(rx, timeout).await;
        if decision.is_none() {
            self.forget(session, request_id);
        }
        decision
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

/// Like [`await_decision`], bounded by an optional `timeout` (ADR-0207 §11,
/// stage 5c: a mode's `question_timeout` governs both `ask_user` questions
/// and parked tool approvals). `None` waits forever, identical to
/// [`await_decision`] — this module knows nothing about modes, only
/// durations, so the `0` = infinite translation happens in the caller
/// (`crate::run_limits::timeout`). `None` in the *return* means the timeout
/// elapsed with no decision delivered; the receiver is then dropped, so a
/// decision landing after the deadline (a slow head, a stale approval)
/// resolves to nothing rather than being silently applied late.
pub async fn await_decision_timed(
    rx: oneshot::Receiver<Decision>,
    timeout: Option<Duration>,
) -> Option<Decision> {
    match timeout {
        None => Some(await_decision(rx).await),
        Some(d) => tokio::time::timeout(d, rx)
            .await
            .ok()
            .map(|res| res.unwrap_or(Decision::Stop)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::ApprovalScope;

    #[test]
    fn snapshot_reports_kind_and_detail_scoped_or_global() {
        let pending = PendingDecisions::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let _rx_a = pending.register(&a, "req-1", "tool", "bash");
        let _rx_b = pending.register(&b, "req-2", "mode", "build");

        let all = pending.snapshot(None);
        assert_eq!(all.len(), 2);

        let only_a = pending.snapshot(Some(&a));
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].kind, "tool");
        assert_eq!(only_a[0].detail, "bash");
    }

    #[tokio::test]
    async fn resolve_delivers_to_the_matching_waiter() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let rx = pending.register(&s, "req-1", "tool", "t");
        pending.resolve(
            &s,
            "req-1",
            Decision::Approve {
                scope: ApprovalScope::Once,
                mode: None,
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
        let rx = pending.register(&s, "req-x", "tool", "t");
        pending.stop_session(&s);
        assert!(matches!(await_decision(rx).await, Decision::Stop));
    }

    #[tokio::test]
    async fn dropped_registry_unwinds_the_waiter_to_stop() {
        let s = SessionId::new("s");
        let rx = {
            let pending = PendingDecisions::default();
            pending.register(&s, "req-1", "tool", "t")
            // `pending` (holding the sender) drops here.
        };
        assert!(matches!(await_decision(rx).await, Decision::Stop));
    }

    #[tokio::test]
    async fn stop_session_unwinds_only_its_own_waiters() {
        let pending = PendingDecisions::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let rx_a = pending.register(&a, "req", "tool", "t");
        let rx_b = pending.register(&b, "req", "tool", "t");
        pending.stop_session(&a);
        assert!(matches!(await_decision(rx_a).await, Decision::Stop));
        // b's waiter is untouched; resolving it still works.
        pending.resolve(
            &b,
            "req",
            Decision::Answer {
                answers: vec![vec!["x".into()]],
            },
        );
        assert!(matches!(
            await_decision(rx_b).await,
            Decision::Answer { .. }
        ));
    }

    #[tokio::test]
    async fn await_decision_timed_none_waits_forever() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let rx = pending.register(&s, "req-1", "tool", "t");
        pending.resolve(
            &s,
            "req-1",
            Decision::Approve {
                scope: ApprovalScope::Once,
                mode: None,
            },
        );
        assert!(matches!(
            await_decision_timed(rx, None).await,
            Some(Decision::Approve { .. })
        ));
    }

    #[tokio::test]
    async fn await_decision_timed_returns_the_decision_within_the_deadline() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let rx = pending.register(&s, "req-1", "tool", "t");
        pending.resolve(&s, "req-1", Decision::Reject { reason: None });
        let decision = await_decision_timed(rx, Some(std::time::Duration::from_secs(5))).await;
        assert!(matches!(decision, Some(Decision::Reject { .. })));
    }

    #[tokio::test]
    async fn await_timed_forgets_the_entry_on_timeout() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let rx = pending.register(&s, "req-1", "tool", "t");
        let d = pending
            .await_timed(&s, "req-1", rx, Some(Duration::from_millis(10)))
            .await;
        assert!(d.is_none());
        assert!(
            pending.lock().is_empty(),
            "a timed-out entry must be removed"
        );
    }

    #[tokio::test]
    async fn await_decision_timed_elapses_to_none_when_nobody_answers() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let rx = pending.register(&s, "req-1", "tool", "t");
        // Nobody ever resolves req-1 — the short deadline must still return,
        // not hang, and must report "no decision" rather than fabricating one.
        let decision = await_decision_timed(rx, Some(std::time::Duration::from_millis(20))).await;
        assert!(decision.is_none());
    }
}
