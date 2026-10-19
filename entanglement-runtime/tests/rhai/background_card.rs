//! A background script's nested card is marked `detached` on the wire
//! (#872, ADR-0237): its turn ends `Idle` while the card stays parked, so a
//! head must tell it from the turn's own cards to keep showing it — and
//! approving it after the turn ended still lets the script finish.

use std::time::Duration;

use entanglement_core::{AgentState, InMsg, OutEvent, SessionId};
use entanglement_runtime::script_ops::ScriptRegistry;

use super::background_park::{approve_and_finish, engine, pending_list};
use super::TempDir;

const WAIT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_background_scripts_card_is_detached_and_outlives_its_turn() {
    let dir = TempDir::new("bg-detached");
    let scripts = ScriptRegistry::new();
    let holly = engine(&dir.path, scripts.clone());
    let sid = SessionId::new("s1");

    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    // The script runs beside its turn, so the card and the turn's `Idle`
    // land in either order: wait for both.
    let (mut card, mut idle) = (None, false);
    while card.is_none() || !idle {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                request_id,
                tool,
                detached,
                ..
            })) if tool.contains("edit") => card = Some((request_id, detached)),
            Ok(Ok(OutEvent::Status { session, state, .. }))
                if session == sid && state == AgentState::Idle =>
            {
                idle = true
            }
            Ok(Ok(_)) => {}
            other => panic!("expected the nested card and an Idle turn: {other:?}"),
        }
    }
    let (request_id, detached) = card.expect("loop exits only with a card");
    assert!(detached, "a background script's card is marked detached");

    // Still parked after the turn's terminal Status, and the listing a
    // reconnecting head rebuilds its view from carries the same flag.
    let listed = pending_list(&holly, &sid, "after-idle").await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_id, request_id);
    assert!(listed[0].detached);

    approve_and_finish(&holly, &scripts, &sid, &sid, request_id, &dir.path).await;
}
