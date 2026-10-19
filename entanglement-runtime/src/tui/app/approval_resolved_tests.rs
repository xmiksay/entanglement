//! The TUI drops a card the runtime resolved elsewhere (#883, ADR-0240):
//! a timeout, another head's decision, a `Stop` racing a late card.

use std::time::Duration;

use entanglement_core::{
    ApprovalKind, ApprovalOutcome, ApprovalScope, EngineConfig, Holly, InMsg, OutEvent, SessionId,
};
use entanglement_runtime::pending::{approval, park_approval_as, PendingDecisions};
use entanglement_runtime::seam::Decision;
use tokio::sync::broadcast::Receiver;

use super::App;
use crate::tui::session_view::ApprovalMode;

fn has_card(app: &App, session: &SessionId) -> bool {
    app.sessions()
        .into_iter()
        .any(|(id, v)| id == session && v.pending_tool_request().is_some())
}

/// Fold every event into `app` until one `stop` accepts (folded too).
async fn fold_until(app: &mut App, sub: &mut Receiver<OutEvent>, stop: impl Fn(&OutEvent) -> bool) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("the awaited event arrives")
            .expect("the engine is up");
        let last = stop(&ev);
        app.handle_out_event(ev);
        if last {
            return;
        }
    }
}

/// A live session `s` whose first turn has finished, its events folded
/// into a fresh app, and a registry that announces on its engine.
async fn settled() -> (Holly, PendingDecisions, App, Receiver<OutEvent>, SessionId) {
    let holly = Holly::spawn(EngineConfig::default());
    let s = SessionId::new("s");
    let mut sub = holly.subscribe();
    let mut app = App::new_for_test(s.clone());
    holly
        .send(InMsg::Spawn {
            session: s.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            prompt: "hi".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .expect("engine accepts Spawn");
    // Settle the turn first: a card parked while the turn still streams
    // could take a seq whose event lands after a later one.
    fold_until(&mut app, &mut sub, |ev| matches!(ev, OutEvent::Done { .. })).await;
    let pending = PendingDecisions::announcing(holly.clone());
    (holly, pending, app, sub, s)
}

fn background_card(s: &SessionId) -> entanglement_core::PendingApproval {
    approval(
        s,
        "x:rhai:edit",
        ApprovalKind::Rhai,
        "edit (rhai)",
        "{}",
        "why",
        None,
    )
}

fn is_resolved(ev: &OutEvent) -> bool {
    matches!(ev, OutEvent::ApprovalResolved { .. })
}

/// A background script's card that expires in an unattended mode leaves
/// the view, through the real `await_timed`.
#[tokio::test]
async fn an_expired_background_card_leaves_the_view() {
    let (holly, pending, mut app, mut sub, s) = settled().await;
    let rx = park_approval_as(&holly, &pending, background_card(&s), "edit", true);
    fold_until(&mut app, &mut sub, |ev| {
        matches!(ev, OutEvent::ToolRequest { .. })
    })
    .await;
    assert!(has_card(&app, &s));

    let d = pending
        .await_timed(&s, "x:rhai:edit", rx, Some(Duration::from_millis(10)))
        .await;
    assert!(d.is_none(), "nobody answered");
    fold_until(&mut app, &mut sub, is_resolved).await;
    assert!(!has_card(&app, &s));
    assert!(matches!(app.approval_mode(), ApprovalMode::Normal));
}

/// Another head approves the card this one shows: the card goes, and the
/// pager and half-typed reason opened on it are reset.
#[tokio::test]
async fn a_card_another_head_answered_leaves_the_view() {
    let (holly, pending, mut app, mut sub, s) = settled().await;
    let _rx = park_approval_as(&holly, &pending, background_card(&s), "edit", true);
    fold_until(&mut app, &mut sub, |ev| {
        matches!(ev, OutEvent::ToolRequest { .. })
    })
    .await;
    app.open_approval_pager();

    // The router's call for the other head's `Approve`.
    let approve = Decision::Approve {
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    };
    pending.resolve(&s, "x:rhai:edit", approve);
    fold_until(&mut app, &mut sub, is_resolved).await;
    assert!(!has_card(&app, &s));
    assert!(!app.showing_approval_pager());
}

/// ADR-0237's Stop race: a card the runtime emitted just before it handled
/// the `Stop`, folded after the local clear, is dropped by its end.
#[tokio::test]
async fn a_card_folded_after_a_stop_is_dropped_by_its_end() {
    let (holly, pending, mut app, mut sub, s) = settled().await;
    let _rx = park_approval_as(&holly, &pending, background_card(&s), "edit", true);
    app.clear_approvals_of(&s);
    pending.stop_session(&s);
    fold_until(&mut app, &mut sub, |ev| {
        matches!(ev, OutEvent::ToolRequest { .. })
    })
    .await;
    assert!(has_card(&app, &s), "the late card shows again");
    fold_until(&mut app, &mut sub, is_resolved).await;
    assert!(!has_card(&app, &s));
}

fn card(s: &SessionId, seq: u64, request_id: &str) -> OutEvent {
    OutEvent::ToolRequest {
        session: s.clone(),
        seq,
        request_id: request_id.into(),
        tool: "edit (rhai)".into(),
        input: "{}".into(),
        reason: None,
        scopes: Vec::new(),
        detached: true,
    }
}

fn resolved(s: &SessionId, seq: u64, request_id: &str) -> OutEvent {
    OutEvent::ApprovalResolved {
        session: s.clone(),
        seq,
        request_id: request_id.into(),
        outcome: ApprovalOutcome::Expired,
    }
}

/// The dropped card was prompted mid reject-reason: the next card is
/// prompted with an empty reason (ADR-0237 §6).
#[test]
fn dropping_the_prompted_card_takes_its_reject_reason() {
    let s = SessionId::new("s");
    let mut app = App::new_for_test(s.clone());
    app.handle_out_event(card(&s, 1, "a"));
    app.handle_out_event(card(&s, 2, "b"));
    app.set_approval_mode(ApprovalMode::EnteringRejectReason {
        request_id: "a".into(),
    });
    app.set_input_text("half typed".into());
    app.handle_out_event(resolved(&s, 3, "a"));
    assert!(matches!(
        app.approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "b"
    ));
    assert_eq!(app.input_text(), "");
    // A queued card's end leaves the prompt alone.
    app.handle_out_event(card(&s, 4, "c"));
    app.handle_out_event(resolved(&s, 5, "c"));
    assert!(matches!(
        app.approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "b"
    ));
}

/// A stale end (seq at or below the view's watermark) must not drop a
/// newer card that reuses the id.
#[test]
fn a_stale_end_keeps_a_newer_card_with_the_same_id() {
    let s = SessionId::new("s");
    let mut app = App::new_for_test(s.clone());
    app.handle_out_event(card(&s, 5, "a"));
    app.handle_out_event(resolved(&s, 4, "a"));
    assert!(has_card(&app, &s));
}

/// An end minted after the session's counter retired (a close or
/// hibernate while parked) carries seq 0: it still drops the card by id,
/// and leaves the watermark where it was.
#[test]
fn a_seq_zero_end_drops_the_card_and_keeps_the_watermark() {
    let s = SessionId::new("s");
    let mut app = App::new_for_test(s.clone());
    app.handle_out_event(card(&s, 5, "a"));
    app.handle_out_event(resolved(&s, 0, "a"));
    assert!(!has_card(&app, &s));
    let view = app.sessions.view_for(&s).expect("view");
    assert_eq!(view.last_seen_seq(), 5);
    app.handle_out_event(card(&s, 6, "b"));
    assert!(has_card(&app, &s), "the next card still folds");
}

/// A `Stop` heal moves a card to the successor silently, so its end names
/// the successor while the predecessor's view still shows it (ADR-0238).
#[test]
fn an_end_on_the_successor_drops_the_predecessors_copy() {
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let mut app = App::new_for_test(p.clone());
    app.handle_out_event(card(&p, 1, "a"));
    app.handle_out_event(OutEvent::SessionStarted {
        session: s.clone(),
        parent: None,
        predecessor: Some(p.clone()),
        agent: "general".into(),
        model: None,
        root: true,
        ts: 0,
        user: None,
    });
    app.handle_out_event(resolved(&s, 1, "a"));
    assert!(!has_card(&app, &p));
}
