//! What every running script reads before each nested call (ADR-0243): its
//! session's current mode, blocking runs included (#898); and its per-run
//! approvals lapse on a mode change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{ApprovalScope, Holly, InMsg, OutEvent, SessionId};

use super::background_follow::{any, approve, edit_card_on, no_card, until};
use super::background_follow_mode::{engine, gate, wait_for_file, wait_on};
use super::TempDir;

type Modes = Arc<Mutex<HashMap<SessionId, String>>>;

/// Move `session` to `mode` and wait until the executor folded it.
async fn fold_mode(holly: &Holly, modes: &Modes, session: &SessionId, mode: &str) {
    holly
        .send(InMsg::SetMode {
            session: session.clone(),
            mode: mode.into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while modes.lock().unwrap().get(session).map(String::as_str) != Some(mode) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the executor never folded {session:?} into {mode}"));
}

/// #898: a blocking script earns a `build` session grant for `f.txt`, the
/// user moves the session to `strict` mid-turn, and the script's next edit
/// of `f.txt` is asked: a grant matches only the mode it was earned in.
#[tokio::test]
async fn a_blocking_script_reads_a_stricter_mode_before_its_next_call() {
    let dir = TempDir::new("live-blocking-mode");
    let (g1, waiting) = (gate(&dir, "g1"), gate(&dir, "waiting"));
    let script = format!(
        r#"edit("f.txt", "", "one\n"); {} edit("f.txt", "one\n", "two\n"); "finished""#,
        wait_on(&g1, Some(&waiting))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let s = SessionId::new("s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(s.clone(), "run")).await.unwrap();
    let card = until(&mut sub, "the first edit's card", any, |ev| {
        edit_card_on(ev, &s)
    })
    .await;
    let OutEvent::ToolRequest { request_id, .. } = card else {
        unreachable!("matched a ToolRequest")
    };
    approve(&holly, &s, &request_id, ApprovalScope::Session).await;
    wait_for_file(&waiting).await;

    fold_mode(&holly, &modes, &s, "strict").await;
    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the second edit asked again", no_card, |ev| {
        edit_card_on(ev, &s)
    })
    .await;
    let f = dir.path.join("f.txt");
    assert_eq!(std::fs::read_to_string(f).unwrap(), "one\n");
}

/// The per-run approval cache answered a card of the old mode: the same
/// edit, approved once under `build`, is asked again after `strict`.
#[tokio::test]
async fn a_scripts_approval_lapses_when_its_mode_changes() {
    let dir = TempDir::new("live-approval-cache");
    let (g1, waiting) = (gate(&dir, "g1"), gate(&dir, "waiting"));
    let edit = r#"edit("f.txt", "", "one\n");"#;
    let script = format!(
        r#"{edit} {} {edit} "finished""#,
        wait_on(&g1, Some(&waiting))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let s = SessionId::new("s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(s.clone(), "run")).await.unwrap();
    let card = until(&mut sub, "the first edit's card", any, |ev| {
        edit_card_on(ev, &s)
    })
    .await;
    let OutEvent::ToolRequest { request_id, .. } = card else {
        unreachable!("matched a ToolRequest")
    };
    approve(&holly, &s, &request_id, ApprovalScope::Once).await;
    wait_for_file(&waiting).await;

    fold_mode(&holly, &modes, &s, "strict").await;
    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the same edit asked again", no_card, |ev| {
        edit_card_on(ev, &s)
    })
    .await;
}
