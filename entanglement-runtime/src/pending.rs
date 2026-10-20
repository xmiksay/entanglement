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
//! ([`stop_session`](PendingDecisions::stop_session)); its end or hibernate
//! spares a background script's ([`stop_attached`](PendingDecisions::stop_attached)).
//! Whatever removes a card tells every head (`OutEvent::ApprovalResolved`,
//! ADR-0240, `resolved`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use entanglement_core::{
    ApprovalKind, ApprovalOutcome, ApprovalScope, Holly, PendingApproval as WireApproval, SessionId,
};
use tokio::sync::oneshot;

use crate::seam::Decision;

mod park;
mod resolved;
mod successor;
#[cfg(test)]
pub(crate) mod test_support;
mod wait;
pub use park::{
    approval, headless_starts, list_pending, park_approval, park_approval_as, scopes_for,
};
pub(crate) use successor::{announce, heal_before, heal_subtree, live_end};
pub use wait::{await_decision, await_decision_timed};

/// What a parked decision is waiting on (#560 P12, ADR-0207 §12): carried
/// alongside the waiter so a listing (`InMsg::ListPending`) can describe a
/// parked approval without a second, drifting registry. `kind` is one of
/// `"tool"` (a permission `Ask`), `"plan"` (`propose_plan`), `"mode"`
/// (`request_mode`), `"rhai"` (a script binding's own `Ask`), or `"ask_user"`
/// (an `ask_user` call — carried here too for completeness, though a
/// handle-less `poll`'s subtree section folds those into its open-questions
/// rows instead of double-reporting them as approvals). `detail` is a short, already
/// human-readable label — the tool name for `"tool"`/`"rhai"`, the requested
/// mode for `"mode"` — never the full call input. `approval` is the full
/// `InMsg::ListPending` payload (#726) for every kind but `"ask_user"`.
#[derive(Clone)]
pub struct Parked {
    pub kind: &'static str,
    pub detail: String,
    pub approval: Option<WireApproval>,
    /// A background (`detached`) script's nested call (ADR-0185): not a
    /// `ToolExec`, so nothing re-offers it — it outlives its session's end
    /// or hibernate ([`PendingDecisions::stop_attached`]).
    pub detached: bool,
    /// The session the card is shown on and stopped with: the map key's own
    /// session, until a compaction moves a detached card to the
    /// successor (ADR-0238). The key never changes, so the predecessor's id
    /// keeps answering it.
    pub home: SessionId,
    /// Every earlier home a move took the card off, oldest first: the
    /// sessions a head may still show a stale copy on, which keep
    /// answering it (ADR-0242).
    pub(crate) via: Vec<SessionId>,
}

type Waiters = HashMap<(SessionId, String), (oneshot::Sender<Decision>, Parked)>;

/// The waiters. A detached card's `home` follows the supervisor's own
/// compaction lineage (`Holly::successor_of`/`predecessors_of`, ADR-0242),
/// so the registry keeps no lineage of its own.
#[derive(Default)]
struct Inner {
    waiters: Waiters,
}

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
/// `Default` reports nothing; [`announcing`](Self::announcing) emits
/// `OutEvent::ApprovalResolved` when a card leaves (ADR-0240).
#[derive(Clone, Default)]
pub struct PendingDecisions {
    inner: Arc<Mutex<Inner>>,
    holly: Option<Holly>,
}

impl PendingDecisions {
    /// A registry that tells every head when a card leaves it, on `holly`.
    pub fn announcing(holly: Holly) -> Self {
        Self {
            inner: Arc::default(),
            holly: Some(holly),
        }
    }

    /// Register a waiter for `(session, request_id)` and return the receiver it
    /// awaits. Call this **before** emitting the request event so the router can
    /// never process the decision before the waiter exists (register-then-emit
    /// closes the race the pre-#156 "subscribe before handing off" discipline
    /// guarded). Only a turn card supersedes a live turn card under the same
    /// key — the earlier waiter resolves to [`Decision::Stop`], the desired
    /// outcome for a re-offered request. Any other park sharing a live
    /// card's key, or its id on its session (Gemini's `name#ordinal` call
    /// ids repeat every round), is refused with a [`Decision::Reject`]
    /// naming why, so a background script's card is never displaced
    /// (ADR-0238).
    /// `kind`/`detail` describe what this park is waiting on (see [`Parked`]).
    pub fn register(
        &self,
        session: &SessionId,
        request_id: &str,
        kind: &'static str,
        detail: impl Into<String>,
    ) -> oneshot::Receiver<Decision> {
        self.insert(session, request_id, kind, detail.into(), None, false)
    }

    /// [`register`](Self::register) for an approval park, keeping its full
    /// [`WireApproval`] for `InMsg::ListPending` (#726). Prefer
    /// [`park_approval`], which also emits the matching `ToolRequest`.
    pub fn register_approval(
        &self,
        approval: WireApproval,
        detail: impl Into<String>,
    ) -> oneshot::Receiver<Decision> {
        self.register_parked(approval, detail, None, |_| {})
    }

    /// [`register_approval`](Self::register_approval), calling `emit` with
    /// the approval as registered while still holding the lock, so a
    /// move can't run between the park and its event (ADR-0238 §6). A
    /// detached card of a compacted session is shown on the live end of the
    /// `lineage` engine's chain ([`successor::live_end`]), so `emit` sees
    /// that session. A refused park emits nothing.
    pub(crate) fn register_parked(
        &self,
        mut approval: WireApproval,
        detail: impl Into<String>,
        lineage: Option<&Holly>,
        emit: impl FnOnce(&WireApproval),
    ) -> oneshot::Receiver<Decision> {
        let detached = approval.detached;
        let kind = match approval.kind {
            ApprovalKind::Tool => "tool",
            ApprovalKind::Plan => "plan",
            ApprovalKind::Mode => "mode",
            ApprovalKind::Rhai => "rhai",
        };
        let key = approval.session.clone();
        let mut guard = self.lock();
        let home = match lineage {
            Some(holly) if detached => successor::live_end(holly, &key),
            _ => key.clone(),
        };
        approval.session = home.clone();
        let parked = Parked {
            kind,
            detail: detail.into(),
            approval: Some(approval.clone()),
            detached,
            home,
            via: Vec::new(),
        };
        match guard.insert(&key, &approval.request_id, parked) {
            Ok((rx, superseded)) => {
                // The old card's end before the new card's announce.
                self.superseded(superseded, &approval.request_id);
                emit(&approval);
                rx
            }
            Err(refused) => refused,
        }
    }

    fn insert(
        &self,
        session: &SessionId,
        request_id: &str,
        kind: &'static str,
        detail: String,
        approval: Option<WireApproval>,
        detached: bool,
    ) -> oneshot::Receiver<Decision> {
        let parked = Parked {
            kind,
            detail,
            approval,
            detached,
            home: session.clone(),
            via: Vec::new(),
        };
        let mut guard = self.lock();
        match guard.insert(session, request_id, parked) {
            Ok((rx, superseded)) => {
                self.superseded(superseded, request_id);
                rx
            }
            Err(refused) => refused,
        }
    }

    /// Every live parked approval whose session `in_scope` accepts, sorted by
    /// `(session, request_id)` (#726). Skips `ask_user` parks (those are
    /// `ListQuestions`' business) and any waiter that already gave up.
    pub fn approvals(&self, in_scope: impl Fn(&SessionId) -> bool) -> Vec<WireApproval> {
        let guard = self.lock();
        let mut list: Vec<WireApproval> = guard
            .listed()
            .into_iter()
            .filter(|(s, _, _)| in_scope(s))
            // A move keeps `approval.session` equal to the home.
            .filter_map(|(_, _, parked)| parked.approval.clone())
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
            .listed()
            .into_iter()
            .filter(|(s, _, _)| session.is_none_or(|f| f == s))
            .map(|(s, rid, parked)| PendingApproval {
                session: s,
                request_id: rid.to_string(),
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
    /// is likewise harmless. An `Approve` scope the card did not offer is
    /// clamped to `Once` ([`clamp_scope`]). A card moved to a compaction
    /// successor answers under its predecessor's id and its new home's
    /// (ADR-0238), and under the id of any home it was moved off (ADR-0242).
    pub fn resolve(&self, session: &SessionId, request_id: &str, decision: Decision) {
        let mut guard = self.lock();
        if let Some((tx, parked)) = guard.remove(session, request_id) {
            // A waiter that gave up got its timeout, not this decision.
            let outcome = if tx.is_closed() {
                ApprovalOutcome::Expired
            } else {
                resolved::outcome_of(&decision)
            };
            self.resolved(&parked, request_id, outcome);
            let _ = tx.send(clamp_scope(decision, &parked, request_id));
        }
    }

    /// Unwind every waiter for `session` with [`Decision::Stop`] (#167) — a
    /// background script's included: a `Stop` is the user's explicit escape
    /// hatch, the TUI clears its card view on it, and an attended park has
    /// no timeout, so a card left parked would hang the script for good.
    /// Core cancels the turn on the same `Stop`, so no `ToolResult` is owed.
    pub fn stop_session(&self, session: &SessionId) {
        self.unwind(session, true);
    }

    /// [`stop_session`](Self::stop_session) for a session that ended or
    /// hibernated (ADR-0232): the cards of its turn go (a resume re-offers a
    /// parked `ToolExec`), but a background (`detached`) script's card stays
    /// — the script outlives the session (ADR-0185/0231) and nothing would
    /// ever ask again.
    pub fn stop_attached(&self, session: &SessionId) {
        self.unwind(session, false);
    }

    fn unwind(&self, session: &SessionId, include_detached: bool) {
        let mut guard = self.lock();
        let keys: Vec<_> = guard
            .waiters
            .iter()
            .filter(|(_, (_, parked))| {
                &parked.home == session && (include_detached || !parked.detached)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Some((tx, parked)) = guard.waiters.remove(&key) {
                self.resolved(&parked, &key.1, ApprovalOutcome::Stopped);
                let _ = tx.send(Decision::Stop);
            }
        }
    }

    /// Drop a park's own entry, `(session, request_id)` exactly, once its
    /// waiter is gone (`await_timed` after a timeout). A live waiter is left
    /// alone, and so is a card moved onto that id (ADR-0238).
    pub fn forget(&self, session: &SessionId, request_id: &str) {
        let mut guard = self.lock();
        if let Some(parked) = guard.forget(session, request_id) {
            self.resolved(&parked, request_id, ApprovalOutcome::Expired);
        }
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

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("pending decisions mutex poisoned")
    }
}

/// An `Approve` whose scope the parked card did not advertise becomes a
/// `Once` (ADR-0230). WHY here, not in each park: `scopes` is the contract
/// a head is shown, and a wire client can send any scope — on a floored
/// write outside the root an unclamped `Always` would persist a durable
/// extra-roots grant the card never offered. Parks with no wire approval
/// (`ask_user`) pass through untouched.
fn clamp_scope(decision: Decision, parked: &Parked, request_id: &str) -> Decision {
    let offered = |scope| {
        parked
            .approval
            .as_ref()
            .is_none_or(|a| a.scopes.contains(&scope))
    };
    match decision {
        Decision::Approve {
            scope,
            mode,
            run_all,
        } if !offered(scope) => {
            tracing::warn!(
                request_id,
                ?scope,
                "approval scope not offered by the card; clamped to once"
            );
            Decision::Approve {
                scope: ApprovalScope::Once,
                mode,
                run_all,
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0230: a scope the card did not offer is delivered as `Once`; an
    /// offered one passes through.
    #[tokio::test]
    async fn an_unoffered_scope_is_clamped_to_once() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let mut floored = approval(&s, "r1", ApprovalKind::Tool, "write", "{}", "why", None);
        floored.scopes = vec![ApprovalScope::Once];
        let launch = approval(&s, "r2", ApprovalKind::Rhai, "rhai", "1", "why", None);
        let approve = |scope| Decision::Approve {
            scope,
            mode: None,
            run_all: false,
        };
        for (req, sent, want) in [
            (floored, ApprovalScope::Always, ApprovalScope::Once),
            (
                launch.clone(),
                ApprovalScope::SessionDir,
                ApprovalScope::Once,
            ),
            (launch, ApprovalScope::Session, ApprovalScope::Session),
        ] {
            let rid = req.request_id.clone();
            let rx = pending.register_approval(req, "t");
            pending.resolve(&s, &rid, approve(sent));
            match await_decision(rx).await {
                Decision::Approve { scope, .. } => assert_eq!(scope, want, "{rid} {sent:?}"),
                _ => panic!("{rid}: not an approval"),
            }
        }
    }

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
                run_all: false,
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

    /// ADR-0232: a session end unwinds its `ToolExec` cards but leaves a
    /// background script's card answerable.
    #[tokio::test]
    async fn stop_attached_keeps_a_detached_scripts_card() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        let attached = pending.insert(&s, "a", "tool", "bash".into(), None, false);
        let detached = pending.insert(&s, "d", "rhai", "bash".into(), None, true);
        pending.stop_attached(&s);
        assert!(matches!(await_decision(attached).await, Decision::Stop));
        pending.resolve(&s, "d", Decision::Reject { reason: None });
        assert!(matches!(
            await_decision(detached).await,
            Decision::Reject { .. }
        ));
        // A `Stop` takes the detached card too.
        let detached = pending.insert(&s, "d2", "rhai", "bash".into(), None, true);
        pending.stop_session(&s);
        assert!(matches!(await_decision(detached).await, Decision::Stop));
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
                run_all: false,
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
            pending.lock().waiters.is_empty(),
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
