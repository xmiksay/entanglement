//! The `serve` wake's queueing (ADR-0247): it is awaited and runs off the
//! read loop, one wake per session, and a connection's frames for the
//! session queue behind the listing in order.

use std::time::Duration;

use entanglement_core::{InMsg, OutEvent, SessionId};

use crate::attended_mode::approve;
use crate::attended_serve::{connect, first, send};
use crate::attended_wake::{asleep_with_card, drain, errors, served};

/// A slow re-offer (its grade takes 400ms): the listing waits for the card
/// rather than answering empty — and a session-less frame sent right behind
/// it is answered first, the wake running off the read loop.
#[tokio::test]
async fn a_slow_reoffer_is_awaited_and_holds_up_nothing_else() {
    let s = served(true).await;
    let sid = SessionId::new("ws-slow");
    let rid = asleep_with_card(&s, &sid).await;

    let mut ws = connect(s.port).await;
    let listing = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    send(&mut ws, &listing).await;
    let sessions = InMsg::ListSessions {
        correlation_id: "q".into(),
    };
    send(&mut ws, &sessions).await;
    let order = first(&mut ws, |e| match e {
        OutEvent::SessionList { .. } => Some("sessions"),
        OutEvent::PendingList { .. } => Some("pending"),
        _ => None,
    })
    .await;
    assert_eq!(order, "sessions", "the wake held up the read loop");
    let listed = first(&mut ws, |e| match e {
        OutEvent::PendingList { approvals, .. } => Some(approvals),
        _ => None,
    })
    .await;
    assert!(
        listed.iter().any(|a| a.request_id == rid),
        "the slow re-offer was not awaited: {listed:?}"
    );
}

/// Two listings back to back start one wake: both are answered with the
/// card, and nothing tries to resume the already-woken session.
#[tokio::test]
async fn two_listings_share_one_wake() {
    let s = served(true).await;
    let sid = SessionId::new("ws-twice");
    let rid = asleep_with_card(&s, &sid).await;

    let mut ws = connect(s.port).await;
    let pending = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    let questions = InMsg::ListQuestions {
        correlation_id: "q".into(),
        session: Some(sid.clone()),
    };
    send(&mut ws, &pending).await;
    send(&mut ws, &questions).await;
    let seen = drain(&mut ws, Duration::from_secs(2)).await;
    assert!(errors(&seen).is_empty(), "{:?}", errors(&seen));
    assert!(
        seen.iter()
            .any(|e| matches!(e, OutEvent::PendingList { approvals, .. }
            if approvals.iter().any(|a| a.request_id == rid))),
        "{seen:?}"
    );
    assert!(seen
        .iter()
        .any(|e| matches!(e, OutEvent::QuestionList { .. })));
}

/// An `Approve` pipelined behind the waking listing waits for the wake: it
/// answers the re-offered card instead of being refused for no card.
#[tokio::test]
async fn a_frame_behind_a_waking_listing_waits_its_turn() {
    let s = served(true).await;
    let sid = SessionId::new("ws-pipe");
    let rid = asleep_with_card(&s, &sid).await;

    let mut ws = connect(s.port).await;
    let listing = InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(sid.clone()),
    };
    send(&mut ws, &listing).await;
    send(&mut ws, &approve(&sid, &rid)).await;
    let seen = drain(&mut ws, Duration::from_secs(3)).await;
    assert!(errors(&seen).is_empty(), "{:?}", errors(&seen));
    assert!(
        seen.iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: echo hi")),
        "{seen:?}"
    );
}

/// A listing from a second connection while the first one's wake is still
/// running waits for that wake instead of starting another (which core
/// would refuse: the session is already live).
/// Kills a mutant that drops `begin_wake`'s `Waking` arm (a second wake's
/// resume is refused with an `Error`).
#[tokio::test]
async fn a_listing_during_another_wake_waits_for_it() {
    let s = served(true).await;
    let sid = SessionId::new("ws-relay");
    let rid = asleep_with_card(&s, &sid).await;

    let mut first_ws = connect(s.port).await;
    let listing = |id: &str| InMsg::ListPending {
        correlation_id: id.into(),
        root: Some(sid.clone()),
    };
    send(&mut first_ws, &listing("first")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(first_ws);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut ws = connect(s.port).await;
    send(&mut ws, &listing("second")).await;
    let seen = drain(&mut ws, Duration::from_secs(2)).await;
    assert!(errors(&seen).is_empty(), "{:?}", errors(&seen));
    assert!(
        // Its own reply (not the first connection's, which every connection
        // sees) lists the card: only a listing that waited for the wake does.
        seen.iter().any(
            |e| matches!(e, OutEvent::PendingList { correlation_id, approvals }
            if correlation_id == "second" && approvals.iter().any(|a| a.request_id == rid))
        ),
        "{seen:?}"
    );
}
