//! Two more facts a following background script reads per call (ADR-0241):
//! the successor's mode limits (not only its name), and whether the
//! successor is live at all.

use entanglement_core::{ApprovalScope, InMsg, OutEvent};

use super::background_follow::{any, approve, compact, executor_caught_up, no_card, until};
use super::background_follow_mode::{
    engine, gate, ids, launch_and_compact, set_mode, wait_for_file, wait_on,
};
use super::TempDir;

/// The re-read mode brings its limits: once the successor is `unattended`
/// (a `question_timeout`), the script's first `Ask` collapses to a denial
/// instead of parking a card no one would answer (ADR-0207 §11).
#[tokio::test]
async fn the_successors_mode_limits_reach_the_script() {
    let dir = TempDir::new("bg-follow-limits");
    let g1 = gate(&dir, "g1");
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, None)
    );
    let (holly, _modes) = engine(&dir.path, script, None);
    let (pred, succ) = ids();
    let mut sub = holly.subscribe();
    launch_and_compact(&holly, &mut sub, &pred, &succ).await;
    set_mode(&holly, &mut sub, &succ, "unattended").await;

    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the script's edit denied", no_card, |ev| {
        matches!(ev, OutEvent::NestedCall { session, tool, is_error: true, .. }
            if session == &succ && tool == "edit")
    })
    .await;
    assert!(!dir.path.join("f.txt").exists(), "the edit never ran");
}

/// A hibernated successor is not followed (ADR-0241 §4): the script's next
/// call parks as the predecessor, so the predecessor's id answers it.
#[tokio::test]
async fn a_hibernated_successor_is_not_followed() {
    let dir = TempDir::new("bg-follow-hib");
    let (g1, started) = (gate(&dir, "g1"), gate(&dir, "started"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, Some(&started))
    );
    let (holly, _modes) = engine(&dir.path, script, None);
    let (pred, succ) = ids();
    let mut sub = holly.subscribe();
    // The first call must already be on the bridge (as the predecessor)
    // before the compaction, so the edit is the first chance to switch.
    holly.send(InMsg::prompt(pred.clone(), "go")).await.unwrap();
    wait_for_file(&started).await;
    compact(&holly, &mut sub, &pred, &succ).await;
    executor_caught_up(&holly, &mut sub, &succ).await;
    holly
        .send(InMsg::HibernateSession {
            session: succ.clone(),
        })
        .await
        .unwrap();
    until(
        &mut sub,
        "the successor hibernated",
        any,
        |ev| matches!(ev, OutEvent::SessionHibernated { session, .. } if session == &succ),
    )
    .await;

    std::fs::write(&g1, "").unwrap();
    let card = until(
        &mut sub,
        "the script's edit card",
        no_card,
        |ev| matches!(ev, OutEvent::ToolRequest { tool, .. } if tool.contains("edit")),
    )
    .await;
    let OutEvent::ToolRequest { request_id, .. } = card else {
        unreachable!("matched a ToolRequest")
    };
    approve(&holly, &pred, &request_id, ApprovalScope::Once).await;
    wait_for_file(&dir.path.join("f.txt")).await;
}
