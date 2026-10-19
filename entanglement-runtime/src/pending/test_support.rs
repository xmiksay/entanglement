//! Shared fixtures for the ADR-0238 heal tests: a background script's card,
//! and a session registered in the supervisor's directory with no executor
//! fold — what a lagged `SessionStarted` leaves.

use std::time::Duration;

use entanglement_core::{ApprovalKind, Holly, InMsg, OutEvent, PendingApproval, SessionId};

/// A background script's nested card parked on `on`.
pub(crate) fn detached_card(on: &SessionId, request_id: &str) -> PendingApproval {
    let mut card = super::approval(
        on,
        request_id,
        ApprovalKind::Rhai,
        "edit (rhai)",
        "{}",
        "why",
        None,
    );
    card.detached = true;
    card
}

/// Spawn `s` (under `parent`, succeeding `predecessor`) and wait for its
/// `SessionStarted`.
pub(crate) async fn spawn_session(
    holly: &Holly,
    s: &SessionId,
    parent: Option<&SessionId>,
    predecessor: Option<&SessionId>,
) {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: s.clone(),
            parent: parent.cloned(),
            predecessor: predecessor.cloned(),
            agent: "general".into(),
            prompt: "hi".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .expect("engine accepts Spawn");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::SessionStarted { session, .. })) if &session == s => return,
            Ok(Ok(_)) => {}
            other => panic!("expected {s} to start: {other:?}"),
        }
    }
}
