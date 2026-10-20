//! What every running script reads before each nested call (ADR-0243): its
//! session's current mode, blocking runs included (#898), and the overlay
//! with each entry dropped once that session switched mode since the
//! launch, even when it switched back, and for ancestors too, through their
//! compactions (#899); and its per-run approvals lapse on a mode change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{ApprovalScope, Holly, InMsg, OutEvent, SessionId, ToolOverlayEntry};

use super::background_follow::{
    any, approve, compact, edit_card_on, executor_caught_up, no_card, until,
};
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

async fn spawn_child(holly: &Holly, parent: &SessionId, child: &SessionId) {
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: "go".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
}

async fn allow_edit(holly: &Holly, session: &SessionId) {
    holly
        .send(InMsg::SetToolOverlay {
            session: session.clone(),
            entries: vec![ToolOverlayEntry::allow("edit")],
        })
        .await
        .unwrap();
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

/// #899 (a): the executor drops the overlay on `build` -> `strict`, and the
/// session goes back to `build` before the script's next call. The mode is
/// the launch's again, but the launch-time `allow` enable is gone.
#[tokio::test]
async fn a_mode_switch_and_its_reversal_drop_the_scripts_overlay_entry() {
    let dir = TempDir::new("live-overlay-reversal");
    let (g1, waiting) = (gate(&dir, "g1"), gate(&dir, "waiting"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, Some(&waiting))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let s = SessionId::new("s");
    let mut sub = holly.subscribe();
    allow_edit(&holly, &s).await;
    holly.send(InMsg::prompt(s.clone(), "go")).await.unwrap();
    wait_for_file(&waiting).await;

    fold_mode(&holly, &modes, &s, "strict").await;
    fold_mode(&holly, &modes, &s, "build").await;
    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the script's edit asked", no_card, |ev| {
        edit_card_on(ev, &s)
    })
    .await;
    assert!(!dir.path.join("f.txt").exists(), "the edit has not run");
}

/// #899 (b): a sub-agent's script is graded under its parent's overlay
/// too. Once the parent switches mode the executor drops the parent's
/// `allow` enable, and so does the child's running script.
#[tokio::test]
async fn an_ancestors_mode_switch_drops_its_overlay_entry_from_the_script() {
    let dir = TempDir::new("live-overlay-ancestor");
    let (g1, waiting) = (gate(&dir, "g1"), gate(&dir, "waiting"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, Some(&waiting))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let (parent, child) = (SessionId::new("parent"), SessionId::new("child"));
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "hello"))
        .await
        .unwrap();
    allow_edit(&holly, &parent).await;
    spawn_child(&holly, &parent, &child).await;
    wait_for_file(&waiting).await;

    fold_mode(&holly, &modes, &parent, "strict").await;
    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the child script's edit asked", no_card, |ev| {
        edit_card_on(ev, &child)
    })
    .await;
    assert!(!dir.path.join("f.txt").exists(), "the edit has not run");
}

/// #899 (b) through a compaction: the parent holding the `allow` enable
/// compacts, so the child's chain now names the successor, which inherited
/// the enable. Once the successor switches mode the executor drops it, and
/// so does the child's running script.
#[tokio::test]
async fn an_ancestors_successor_switching_mode_drops_the_inherited_entry() {
    let dir = TempDir::new("live-overlay-ancestor-compact");
    let (g1, waiting) = (gate(&dir, "g1"), gate(&dir, "waiting"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, Some(&waiting))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let (parent, succ, child) = (
        SessionId::new("parent"),
        SessionId::new("parent2"),
        SessionId::new("child"),
    );
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "hello"))
        .await
        .unwrap();
    allow_edit(&holly, &parent).await;
    spawn_child(&holly, &parent, &child).await;
    wait_for_file(&waiting).await;

    compact(&holly, &mut sub, &parent, &succ).await;
    executor_caught_up(&holly, &mut sub, &succ).await;
    fold_mode(&holly, &modes, &succ, "strict").await;
    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the child script's edit asked", no_card, |ev| {
        edit_card_on(ev, &child)
    })
    .await;
    assert!(!dir.path.join("f.txt").exists(), "the edit has not run");
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

/// The HIGH case of review round 2: the parent switches mode and back
/// before compacting. The executor dropped its overlay on the first switch,
/// so core hands the successor none; the child's script must not carry the
/// launch-time enable over to the successor either.
#[tokio::test]
async fn a_parents_switch_and_reversal_before_compacting_drops_the_entry() {
    let dir = TempDir::new("live-overlay-reversal-compact");
    let (g1, waiting) = (gate(&dir, "g1"), gate(&dir, "waiting"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, Some(&waiting))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let (parent, succ, child) = (
        SessionId::new("parent"),
        SessionId::new("parent2"),
        SessionId::new("child"),
    );
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "hello"))
        .await
        .unwrap();
    allow_edit(&holly, &parent).await;
    spawn_child(&holly, &parent, &child).await;
    wait_for_file(&waiting).await;

    fold_mode(&holly, &modes, &parent, "strict").await;
    fold_mode(&holly, &modes, &parent, "build").await;
    compact(&holly, &mut sub, &parent, &succ).await;
    executor_caught_up(&holly, &mut sub, &succ).await;
    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the child script's edit asked", no_card, |ev| {
        edit_card_on(ev, &child)
    })
    .await;
    assert!(!dir.path.join("f.txt").exists(), "the edit has not run");
}

/// Review round 3's probe: two compactions between two calls of the child's
/// script. The middle successor switches mode and back, so the executor
/// dropped its inherited enable and the last successor inherits none; the
/// script's entry for the parent's lineage goes too.
#[tokio::test]
async fn a_middle_successors_switch_and_reversal_drops_the_entry() {
    let dir = TempDir::new("live-overlay-two-hops");
    let (g1, waiting) = (gate(&dir, "g1"), gate(&dir, "waiting"));
    let script = format!(
        r#"{} edit("f.txt", "", "one\n"); "finished""#,
        wait_on(&g1, Some(&waiting))
    );
    let (holly, modes) = engine(&dir.path, script, None);
    let [p, p2, p3, child] = ["parent", "parent2", "parent3", "child"].map(SessionId::new);
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(p.clone(), "hello")).await.unwrap();
    allow_edit(&holly, &p).await;
    spawn_child(&holly, &p, &child).await;
    wait_for_file(&waiting).await;

    compact(&holly, &mut sub, &p, &p2).await;
    executor_caught_up(&holly, &mut sub, &p2).await;
    fold_mode(&holly, &modes, &p2, "strict").await;
    fold_mode(&holly, &modes, &p2, "build").await;
    compact(&holly, &mut sub, &p2, &p3).await;
    executor_caught_up(&holly, &mut sub, &p3).await;
    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the child script's edit asked", no_card, |ev| {
        edit_card_on(ev, &child)
    })
    .await;
    assert!(!dir.path.join("f.txt").exists(), "the edit has not run");
}
