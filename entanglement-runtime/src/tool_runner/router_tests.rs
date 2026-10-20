//! The router's ADR-0238 heals, after a directory-only setup: the successor
//! exists in the supervisor's directory, but no lifecycle fold ever moved
//! its predecessor's card — exactly what a lagged `SessionStarted` leaves.

use std::time::Duration;

use entanglement_core::{
    ApprovalScope, EngineConfig, Holly, InMsg, OutEvent, PendingApproval, SessionId,
};
use tokio::sync::broadcast::{error::TryRecvError, Receiver};
use tokio::sync::oneshot;

use super::Router;
use crate::pending::test_support::{detached_card, spawn_session};
use crate::seam::Decision;

pub(super) fn router(holly: &Holly) -> Router {
    Router {
        cancels: Default::default(),
        hooks: Default::default(),
        pending: Default::default(),
        open_questions: Default::default(),
        jobs: Default::default(),
        agents: Default::default(),
        scripts: Default::default(),
        holly: holly.clone(),
    }
}

/// Engine + router with `succ` registered as `pred`'s compaction successor
/// and a background script's card parked on `pred`.
pub(super) async fn setup() -> (Holly, Router, SessionId, oneshot::Receiver<Decision>) {
    let holly = Holly::spawn(EngineConfig::default());
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    spawn_session(&holly, &pred, None, None).await;
    spawn_session(&holly, &succ, None, Some(&pred)).await;
    let r = router(&holly);
    let rx = park_detached(&r, &pred);
    (holly, r, succ, rx)
}

fn park_detached(r: &Router, on: &SessionId) -> oneshot::Receiver<Decision> {
    r.pending
        .register_approval(detached_card(on, "x-1:rhai:edit"), "edit")
}

/// What the router emitted since `sub` subscribed: the sessions each
/// `ToolRequest` named, and a `PendingList` reply's approvals.
fn drain(sub: &mut Receiver<OutEvent>) -> (Vec<SessionId>, Vec<PendingApproval>) {
    let (mut on, mut listed) = (Vec::new(), Vec::new());
    loop {
        match sub.try_recv() {
            Ok(OutEvent::ToolRequest { session, .. }) => on.push(session),
            Ok(OutEvent::PendingList { approvals, .. }) => listed = approvals,
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(_) => return (on, listed),
        }
    }
}

#[tokio::test]
async fn list_pending_rooted_at_the_successor_heals_and_announces() {
    let (holly, r, succ, _rx) = setup().await;
    let mut sub = holly.subscribe();
    r.route(InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(succ.clone()),
    });
    let (on, listed) = drain(&mut sub);
    assert_eq!(
        on,
        std::slice::from_ref(&succ),
        "the moved card is announced"
    );
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].session, succ);
}

#[tokio::test]
async fn a_decision_on_the_successor_heals_without_announcing_its_card() {
    let (holly, r, succ, rx) = setup().await;
    let mut sub = holly.subscribe();
    r.route(InMsg::Approve {
        session: succ.clone(),
        request_id: "x-1:rhai:edit".into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    });
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx).await,
        Ok(Ok(Decision::Approve { .. }))
    ));
    assert!(
        drain(&mut sub).0.is_empty(),
        "no dead card on the successor"
    );
}

#[tokio::test]
async fn a_stop_on_the_successor_heals_silently_and_unwinds_the_card() {
    let (holly, r, succ, rx) = setup().await;
    let mut sub = holly.subscribe();
    r.route(InMsg::Stop { session: succ });
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx).await,
        Ok(Ok(Decision::Stop))
    ));
    assert!(drain(&mut sub).0.is_empty());
}

/// F6: the rooted heal walks the whole sub-tree, so a compacted sub-agent's
/// card is listed under its successor.
#[tokio::test]
async fn list_pending_heals_a_compacted_sub_agent_under_the_root() {
    let holly = Holly::spawn(EngineConfig::default());
    let (root, child, child2) = (
        SessionId::new("root"),
        SessionId::new("child"),
        SessionId::new("child2"),
    );
    spawn_session(&holly, &root, None, None).await;
    spawn_session(&holly, &child, Some(&root), None).await;
    let r = router(&holly);
    let _status = r
        .agents
        .register(child.clone(), root.clone(), "general".into());
    spawn_session(&holly, &child2, None, Some(&child)).await;
    let _rx = park_detached(&r, &child);

    let mut sub = holly.subscribe();
    r.route(InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(root.clone()),
    });
    let (on, listed) = drain(&mut sub);
    assert_eq!(on, std::slice::from_ref(&child2));
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].session, child2);
}

/// The `(session, input)` of each `ToolRequest` emitted since `sub`
/// subscribed.
fn requests(sub: &mut Receiver<OutEvent>) -> Vec<(SessionId, String)> {
    let mut seen = Vec::new();
    loop {
        match sub.try_recv() {
            Ok(OutEvent::ToolRequest { session, input, .. }) => seen.push((session, input)),
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(_) => return seen,
        }
    }
}

fn card_with(on: &SessionId, input: &str) -> PendingApproval {
    let mut card = detached_card(on, "x-1:rhai:edit");
    card.input = input.into();
    card
}

async fn decided(rx: oneshot::Receiver<Decision>) -> Decision {
    tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("the waiter was decided")
        .expect("the sender answered")
}

/// A predecessor's card parked before the compaction, with no fold run.
async fn parked_before_compaction() -> (Holly, Router, SessionId, oneshot::Receiver<Decision>) {
    let holly = Holly::spawn(EngineConfig::default());
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    spawn_session(&holly, &pred, None, None).await;
    let r = router(&holly);
    let moved =
        crate::pending::park_approval_as(&holly, &r.pending, card_with(&pred, "P"), "e", true);
    spawn_session(&holly, &succ, None, Some(&pred)).await;
    (holly, r, succ, moved)
}

/// H1: the successor's own park of the moved card's id heals first, so the
/// moved card is on the successor and the park is refused. The one card
/// shown is the moved one, and approving it on the successor reaches it —
/// never a card the user did not see.
#[tokio::test]
async fn the_successors_park_heals_first_and_is_refused() {
    let (holly, r, succ, moved) = parked_before_compaction().await;
    let mut sub = holly.subscribe();
    let own =
        crate::pending::park_approval_as(&holly, &r.pending, card_with(&succ, "S"), "e", true);
    assert!(matches!(
        decided(own).await,
        Decision::Reject { reason: Some(why) } if why.contains("already parked")
    ));
    r.route(InMsg::ListPending {
        correlation_id: "c".into(),
        root: Some(succ.clone()),
    });
    let shown = requests(&mut sub);
    assert_eq!(
        shown,
        [(succ.clone(), "P".to_string())],
        "only the moved card"
    );
    r.route(InMsg::Approve {
        session: succ,
        request_id: "x-1:rhai:edit".into(),
        scope: ApprovalScope::Session,
        mode: None,
        run_all: false,
    });
    assert!(matches!(decided(moved).await, Decision::Approve { .. }));
}

/// The same holds for a turn card of the successor's with that id.
#[tokio::test]
async fn the_successors_turn_card_with_a_moved_id_is_refused() {
    let (holly, r, succ, moved) = parked_before_compaction().await;
    let mut turn = card_with(&succ, "S");
    turn.kind = entanglement_core::ApprovalKind::Tool;
    let own = crate::pending::park_approval(&holly, &r.pending, turn, "e");
    assert!(matches!(decided(own).await, Decision::Reject { .. }));
    r.route(InMsg::Reject {
        session: succ,
        request_id: "x-1:rhai:edit".into(),
        reason: None,
    });
    assert!(matches!(decided(moved).await, Decision::Reject { .. }));
}

/// A detached park on a session that already has a successor lands on the
/// successor, so the predecessor never shows a card a later move takes.
#[tokio::test]
async fn a_late_predecessor_park_lands_on_the_successor() {
    let holly = Holly::spawn(EngineConfig::default());
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    spawn_session(&holly, &pred, None, None).await;
    spawn_session(&holly, &succ, None, Some(&pred)).await;
    let r = router(&holly);
    let mut sub = holly.subscribe();
    let late =
        crate::pending::park_approval_as(&holly, &r.pending, card_with(&pred, "P"), "e", true);
    assert_eq!(requests(&mut sub), [(succ.clone(), "P".to_string())]);
    r.route(InMsg::Stop { session: succ });
    assert!(matches!(decided(late).await, Decision::Stop));
}

/// A park never homes on, or moves to, a successor that is not live
/// (`live_end`; not yet started, or already ended), so the card is announced
/// with a real seq on the session where it is, never on one that can't show
/// it.
#[tokio::test]
async fn a_park_never_moves_onto_a_successor_that_is_not_live() {
    let holly = Holly::spawn(EngineConfig::default());
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    spawn_session(&holly, &pred, None, None).await;
    spawn_session(&holly, &succ, None, Some(&pred)).await;
    let mut ended = holly.subscribe();
    holly
        .send(InMsg::CloseSession {
            session: succ.clone(),
        })
        .await
        .expect("engine accepts CloseSession");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ended.recv()).await {
            Ok(Ok(OutEvent::SessionEnded { session, .. })) if session == succ => break,
            Ok(Ok(_)) => {}
            other => panic!("expected {succ} to end: {other:?}"),
        }
    }
    assert!(!holly.is_live(&succ));
    let r = router(&holly);
    let mut sub = holly.subscribe();
    let _card =
        crate::pending::park_approval_as(&holly, &r.pending, card_with(&pred, "P"), "e", true);
    let mut seqs = Vec::new();
    while let Ok(ev) = sub.try_recv() {
        if let OutEvent::ToolRequest { session, seq, .. } = ev {
            seqs.push((session, seq));
        }
    }
    assert_eq!(seqs.len(), 1, "{seqs:?}");
    assert_eq!(seqs[0].0, pred);
    assert!(seqs[0].1 > 0, "a real seq");
    assert_eq!(r.pending.snapshot(None)[0].session, pred);
}

/// A stale decision on a session that ended moves nothing onto it: the
/// session has left the directory, so its heal finds no predecessor.
#[tokio::test]
async fn a_decision_on_an_ended_session_finds_nothing_to_move() {
    let (holly, r, succ, rx) = setup().await;
    holly
        .send(InMsg::CloseSession {
            session: succ.clone(),
        })
        .await
        .expect("engine accepts CloseSession");
    tokio::time::timeout(Duration::from_secs(5), async {
        while holly.is_live(&succ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the successor ends");
    r.route(InMsg::Approve {
        session: succ,
        request_id: "x-1:rhai:edit".into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    });
    assert_eq!(r.pending.snapshot(None)[0].session.0, "pred");
    drop(rx);
}

/// A decision on an intermediate successor whose card moves on past it
/// (its lineage already continues) still announces the card there: the
/// card was never shown on that session, so the decision does not answer
/// it (ADR-0242).
#[tokio::test]
async fn a_decision_heal_announces_a_card_moved_past_its_session() {
    let holly = Holly::spawn(EngineConfig::default());
    let (pred, succ, succ2) = (
        SessionId::new("pred"),
        SessionId::new("succ"),
        SessionId::new("succ2"),
    );
    spawn_session(&holly, &pred, None, None).await;
    spawn_session(&holly, &succ, None, Some(&pred)).await;
    spawn_session(&holly, &succ2, None, Some(&succ)).await;
    let r = router(&holly);
    let _rx = park_detached(&r, &pred);
    let mut sub = holly.subscribe();
    r.route(InMsg::Approve {
        session: succ,
        request_id: "x-1:rhai:edit".into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    });
    assert_eq!(drain(&mut sub).0, std::slice::from_ref(&succ2));
    assert_eq!(r.pending.snapshot(None)[0].session, succ2, "still waiting");
}
