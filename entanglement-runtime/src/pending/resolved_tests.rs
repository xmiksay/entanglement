use std::time::Duration;

use entanglement_core::{ApprovalKind, EngineConfig, Holly, SessionId};
use tokio::sync::broadcast;

use super::super::test_support::detached_card;
use super::super::{approval, park, Resolution};
use super::*;

/// A registry that announces, and a subscriber that sees what it emits.
fn announcing() -> (PendingDecisions, Holly, broadcast::Receiver<OutEvent>) {
    let holly = Holly::spawn(EngineConfig::default());
    let sub = holly.subscribe();
    (PendingDecisions::announcing(holly.clone()), holly, sub)
}

/// Every card event emitted so far, as `(kind, session, request_id, outcome)`.
fn drained(sub: &mut broadcast::Receiver<OutEvent>) -> Vec<(String, String, String, String)> {
    let mut seen = Vec::new();
    while let Ok(ev) = sub.try_recv() {
        match ev {
            OutEvent::ToolRequest {
                session,
                request_id,
                ..
            } => seen.push(("request".into(), session.0, request_id, String::new())),
            OutEvent::ApprovalResolved {
                session,
                request_id,
                outcome,
                ..
            } => seen.push((
                "resolved".into(),
                session.0,
                request_id,
                format!("{outcome:?}"),
            )),
            _ => {}
        }
    }
    seen
}

fn resolved(session: &str, request_id: &str, outcome: &str) -> (String, String, String, String) {
    (
        "resolved".into(),
        session.into(),
        request_id.into(),
        outcome.into(),
    )
}

fn request(session: &str, request_id: &str) -> (String, String, String, String) {
    (
        "request".into(),
        session.into(),
        request_id.into(),
        String::new(),
    )
}

fn card(s: &SessionId, request_id: &str) -> entanglement_core::PendingApproval {
    approval(s, request_id, ApprovalKind::Tool, "bash", "ls", "why", None)
}

fn approve() -> Decision {
    Decision::Approve {
        scope: entanglement_core::ApprovalScope::Once,
        mode: None,
        run_all: false,
    }
}

/// Approve and reject: the event follows the card's `ToolRequest`.
#[tokio::test]
async fn a_decision_reports_its_outcome_after_the_card() {
    let (pending, holly, mut sub) = announcing();
    let s = SessionId::new("s");
    for (rid, decision, outcome) in [
        ("a", approve(), "Approved"),
        ("r", Decision::Reject { reason: None }, "Rejected"),
    ] {
        let _rx = park::park_approval(&holly, &pending, card(&s, rid), "bash");
        let _ = pending.resolve(&s, rid, decision);
        assert_eq!(
            drained(&mut sub),
            [request("s", rid), resolved("s", rid, outcome)]
        );
    }
    // A decision for nothing parked reports nothing.
    let _ = pending.resolve(&s, "a", approve());
    assert!(drained(&mut sub).is_empty());
}

#[tokio::test]
async fn a_timeout_reports_expired_once() {
    let (pending, holly, mut sub) = announcing();
    let s = SessionId::new("s");
    let rx = park::park_approval(&holly, &pending, card(&s, "t"), "bash");
    let d = pending
        .await_timed(
            &s,
            "t",
            rx,
            crate::attendance::Expiry::after(Some(Duration::from_millis(10))),
        )
        .await;
    assert!(d.is_none());
    // A late decision finds no card, so nothing more is reported.
    let _ = pending.resolve(&s, "t", approve());
    assert_eq!(
        drained(&mut sub),
        [request("s", "t"), resolved("s", "t", "Expired")]
    );
}

/// `Stop` takes every card; an end or hibernate only the turn's.
#[tokio::test]
async fn an_unwind_reports_stopped_for_each_card_it_takes() {
    let (pending, holly, mut sub) = announcing();
    let s = SessionId::new("s");
    let _turn = park::park_approval(&holly, &pending, card(&s, "turn"), "bash");
    let _bg = park::park_approval_as(&holly, &pending, detached_card(&s, "bg"), "edit", true);
    drained(&mut sub);
    pending.stop_attached(&s);
    assert_eq!(drained(&mut sub), [resolved("s", "turn", "Stopped")]);
    pending.stop_session(&s);
    assert_eq!(drained(&mut sub), [resolved("s", "bg", "Stopped")]);
}

/// A re-offer superseding a turn card ends the old card before it shows
/// the new one, so a head dropping by id keeps the new card.
#[tokio::test]
async fn a_superseded_card_is_stopped_before_the_new_card() {
    let (pending, holly, mut sub) = announcing();
    let s = SessionId::new("s");
    let _old = park::park_approval(&holly, &pending, card(&s, "x"), "bash");
    let _new = park::park_approval(&holly, &pending, card(&s, "x"), "bash");
    assert_eq!(
        drained(&mut sub),
        [
            request("s", "x"),
            resolved("s", "x", "Stopped"),
            request("s", "x")
        ]
    );
}

/// A moved card ends on its home (ADR-0238), whichever key answered it.
#[tokio::test]
async fn a_moved_card_is_reported_on_its_home() {
    let (pending, _holly, mut sub) = announcing();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let _rx = pending.register_approval(detached_card(&p, "d"), "edit");
    pending.reassign_detached(std::slice::from_ref(&p), &s, |_, _| {});
    let _ = pending.resolve(&p, "d", approve());
    assert_eq!(drained(&mut sub), [resolved("s", "d", "Approved")]);
}

/// A decision reaching a waiter that already gave up (its timeout fired,
/// `forget` not yet run) reports what the waiter got: `Expired`.
#[tokio::test]
async fn a_decision_for_a_gone_waiter_reports_expired() {
    let (pending, _holly, mut sub) = announcing();
    let s = SessionId::new("s");
    drop(pending.register_approval(card(&s, "g"), "bash"));
    let _ = pending.resolve(&s, "g", approve());
    assert_eq!(drained(&mut sub), [resolved("s", "g", "Expired")]);
}

/// A moved card's timeout cleanup (`forget` under its key session) still
/// reports on its home.
#[tokio::test]
async fn a_moved_cards_expiry_is_reported_on_its_home() {
    let (pending, _holly, mut sub) = announcing();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let rx = pending.register_approval(detached_card(&p, "d"), "edit");
    pending.reassign_detached(std::slice::from_ref(&p), &s, |_, _| {});
    drop(rx);
    pending.forget(&p, "d");
    assert_eq!(drained(&mut sub), [resolved("s", "d", "Expired")]);
}

/// An `ask_user` park is no approval card: its end is not reported.
#[tokio::test]
async fn a_question_is_never_reported() {
    let (pending, _holly, mut sub) = announcing();
    let s = SessionId::new("s");
    let _q = pending.register(&s, "q", "ask_user", "q");
    let _ = pending.resolve(&s, "q", Decision::Retract);
    let _q = pending.register(&s, "q2", "ask_user", "q2");
    pending.stop_session(&s);
    assert!(drained(&mut sub).is_empty());
}

#[test]
fn the_outcome_names_the_decision() {
    assert_eq!(outcome_of(&approve()), ApprovalOutcome::Approved);
    assert_eq!(
        outcome_of(&Decision::Reject { reason: None }),
        ApprovalOutcome::Rejected
    );
    assert_eq!(outcome_of(&Decision::Stop), ApprovalOutcome::Stopped);
}

/// #895, ADR-0245: a question decision aimed at an approval card is
/// refused, not an unwind. The card stays parked and listed, reports no
/// end, and its own decision still reaches it.
#[tokio::test]
async fn a_question_decision_for_an_approval_card_is_refused() {
    let (pending, _holly, mut sub) = announcing();
    let s = SessionId::new("s");
    let mut rx = pending.register_approval(card(&s, "r"), "bash");
    for d in [
        Decision::Retract,
        Decision::Answer { answers: vec![] },
        Decision::Replace { questions: vec![] },
    ] {
        assert_eq!(
            pending.resolve(&s, "r", d),
            Resolution::Misaimed { kind: "tool" }
        );
    }
    assert!(rx.try_recv().is_err(), "still waiting");
    assert_eq!(pending.approvals(|_| true).len(), 1, "still listed");
    assert!(drained(&mut sub).is_empty(), "no end reported");
    assert_eq!(pending.resolve(&s, "r", approve()), Resolution::Delivered);
    assert!(matches!(rx.await, Ok(Decision::Approve { .. })));
    assert_eq!(drained(&mut sub), [resolved("s", "r", "Approved")]);
}

/// The reverse: an `Approve`/`Reject` aimed at an `ask_user` question is
/// refused, and the question still takes its answer.
#[tokio::test]
async fn an_approval_decision_for_a_question_is_refused() {
    let pending = PendingDecisions::default();
    let s = SessionId::new("s");
    let mut rx = pending.register(&s, "q", "ask_user", "q");
    for d in [approve(), Decision::Reject { reason: None }] {
        assert_eq!(
            pending.resolve(&s, "q", d),
            Resolution::Misaimed { kind: "ask_user" }
        );
    }
    assert!(rx.try_recv().is_err(), "still waiting");
    assert_eq!(pending.resolve(&s, "absent", approve()), Resolution::Absent);
    assert_eq!(
        pending.resolve(&s, "q", Decision::Retract),
        Resolution::Delivered
    );
    assert!(matches!(rx.await, Ok(Decision::Retract)));
}

/// A misaimed decision for a card whose waiter already gave up takes the
/// dead entry like any late decision, reporting `Expired`, not a refusal.
#[tokio::test]
async fn a_misaimed_decision_for_a_gone_waiter_is_no_refusal() {
    let (pending, _holly, mut sub) = announcing();
    let s = SessionId::new("s");
    drop(pending.register_approval(card(&s, "g"), "bash"));
    assert_eq!(
        pending.resolve(&s, "g", Decision::Retract),
        Resolution::Delivered
    );
    assert_eq!(drained(&mut sub), [resolved("s", "g", "Expired")]);
}
