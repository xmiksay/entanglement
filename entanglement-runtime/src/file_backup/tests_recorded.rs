//! Changes that record the live tree they were made in (ADR-0236): a revert
//! takes exactly those naming its target, whatever the logs' order or clock.

use std::time::Duration;

use entanglement_core::{AgentState, EngineConfig, Holly, InMsg, OutEvent, SessionId};

use super::tests::Fixture;
use super::tests_adopted::{exec, log, prompt, sid, started, LiveTree};
use super::tracker::ancestry_walk;
use super::{holly_ancestry, lock, with_exec_seq, FileBackups};
use crate::session_store::LogPayload;

fn pc(child: &SessionId, parent: &SessionId) -> LogPayload {
    LogPayload::Out(OutEvent::ParentChanged {
        session: child.clone(),
        parent: parent.clone(),
    })
}

fn started_output(session: &SessionId, seq: u64) -> LogPayload {
    LogPayload::Out(OutEvent::ToolOutput {
        session: session.clone(),
        seq,
        request_id: "req".into(),
        tool: "rhai".into(),
        output: "[background script started]".into(),
        content: Vec::new(),
        is_error: false,
        duration_ms: None,
        exit_code: None,
        envelope: None,
    })
}

/// `pred` (a root) spawned `child`; both live and logged under `pred`.
fn base(f: &Fixture) -> (LiveTree, SessionId, SessionId, SessionId) {
    let tree = LiveTree::install(&f.backups);
    let (pred, succ, child) = (sid("pred"), sid("succ"), sid("child"));
    f.backups.note_root(pred.clone(), pred.clone());
    f.backups.note_root(child.clone(), pred.clone());
    f.backups.note_root(succ.clone(), succ.clone());
    tree.set(&pred, None);
    tree.set(&child, Some(&pred));
    log(f, &pred, &pred, started(&pred, None, None));
    log(f, &pred, &pred, prompt(&pred, "delegate"));
    log(f, &pred, &child, started(&child, Some(&pred), None));
    log(f, &pred, &child, prompt(&child, "task"));
    (tree, pred, succ, child)
}

/// The compaction: the directory moves `child` first, then the logs follow.
fn compact(f: &Fixture, tree: &LiveTree, pred: &SessionId, succ: &SessionId, child: &SessionId) {
    tree.set(succ, None);
    tree.set(child, Some(succ));
    log(f, pred, child, pc(child, succ));
    log(f, succ, succ, started(succ, None, Some(pred)));
    log(f, succ, child, LogPayload::Adopted { from: pred.clone() });
    log(f, succ, succ, prompt(succ, "[Conversation summary]"));
}

/// A write on call `seq`'s task — what a background `rhai` script does.
async fn write_on_task(f: &Fixture, session: &SessionId, seq: u64, rel: &str) {
    let abs = f.path(rel);
    std::fs::write(&abs, rel).unwrap();
    with_exec_seq(seq, async {
        f.backups
            .record(session, "req", &abs, rel, None, Some(rel.as_bytes()))
    })
    .await;
}

/// A background script answers "started" at once and writes on: its writes
/// on each side of the move belong to the tree it was in at the time.
#[tokio::test]
async fn a_background_scripts_writes_follow_the_move() {
    let f = Fixture::new();
    let (tree, pred, succ, child) = base(&f);
    log(&f, &pred, &child, exec(&child, 2));
    log(&f, &pred, &child, started_output(&child, 3));
    write_on_task(&f, &child, 2, "pre.txt").await;
    compact(&f, &tree, &pred, &succ, &child);
    write_on_task(&f, &child, 2, "bg.txt").await;

    let r = f.backups.revert(&pred, Some(0)).unwrap();
    assert_eq!(
        r.restored,
        vec!["pre.txt"],
        "the predecessor keeps its era only"
    );
    assert!(f.read("bg.txt").is_some());
    let turns = f.backups.turns(&succ).unwrap();
    assert_eq!((turns.len(), turns[0].turn), (1, 1), "{turns:?}");
    let r = f.backups.revert(&succ, Some(0)).unwrap();
    assert_eq!(r.restored, vec!["bg.txt"]);
}

/// A write racing the move goes by the tree the directory showed when it
/// was recorded, not by where its records land in the logs.
#[test]
fn a_write_racing_the_move_goes_by_its_recorded_tree() {
    let f = Fixture::new();
    let (tree, pred, succ, child) = base(&f);
    // Recorded before the directory moved, though its call is logged after
    // the move record.
    f.change(&child, "early.txt", Some(b"x"));
    tree.set(&succ, None);
    tree.set(&child, Some(&succ));
    // Recorded after the directory moved, before any of the move's records.
    f.change(&child, "late.txt", Some(b"x"));
    log(&f, &pred, &child, pc(&child, &succ));
    log(&f, &pred, &child, exec(&child, 2));
    log(&f, &succ, &succ, started(&succ, None, Some(&pred)));
    log(
        &f,
        &succ,
        &child,
        LogPayload::Adopted { from: pred.clone() },
    );
    log(&f, &succ, &succ, prompt(&succ, "[Conversation summary]"));

    assert_eq!(
        f.backups.revert(&succ, Some(0)).unwrap().restored,
        vec!["late.txt"]
    );
    assert_eq!(
        f.backups.revert(&pred, Some(0)).unwrap().restored,
        vec!["early.txt"]
    );
}

/// Changes from before #870 (no recorded tree) sit beside recorded ones in
/// one manifest; the old ones are still placed by the logs.
#[test]
fn legacy_changes_are_still_placed_by_the_logs() {
    let f = Fixture::new();
    let (pred, succ, child) = (sid("pred"), sid("succ"), sid("child"));
    let legacy = FileBackups::new(f.store.clone());
    legacy.note_root(child.clone(), pred.clone());
    log(&f, &pred, &pred, started(&pred, None, None));
    log(&f, &pred, &pred, prompt(&pred, "delegate"));
    log(&f, &pred, &child, started(&child, Some(&pred), None));
    log(&f, &pred, &child, prompt(&child, "task"));
    log(&f, &pred, &child, exec(&child, 1));
    let record = |b: &FileBackups, rel: &str| {
        std::fs::write(f.path(rel), rel).unwrap();
        b.record(&child, "req", &f.path(rel), rel, None, Some(rel.as_bytes()));
    };
    record(&legacy, "old_pre.txt");
    log(&f, &pred, &child, pc(&child, &succ));
    log(&f, &succ, &succ, started(&succ, None, Some(&pred)));
    log(
        &f,
        &succ,
        &child,
        LogPayload::Adopted { from: pred.clone() },
    );
    log(&f, &succ, &succ, prompt(&succ, "[Conversation summary]"));
    log(&f, &pred, &child, exec(&child, 2));
    record(&legacy, "old_post.txt");

    let tree = LiveTree::install(&f.backups);
    tree.set(&succ, None);
    tree.set(&child, Some(&succ));
    f.backups.note_root(child.clone(), pred.clone());
    f.backups.note_root(succ.clone(), succ.clone());
    record(&f.backups, "new_post.txt");

    let mut r = f.backups.revert(&succ, Some(0)).unwrap().restored;
    r.sort();
    assert_eq!(r, vec!["new_post.txt", "old_post.txt"]);
    assert!(f.read("old_pre.txt").is_some());
}

/// Moved twice, with a grandchild spawned before the first move: each
/// write is its own era's tree's, and turns follow the target's prompts.
#[test]
fn twice_moved_child_and_grandchild_follow_their_recorded_trees() {
    let f = Fixture::new();
    let (tree, pred, succ, child) = base(&f);
    let (succ2, gk) = (sid("succ2"), sid("gk"));
    f.backups.note_root(succ2.clone(), succ2.clone());
    f.backups.note_root(gk.clone(), pred.clone());
    tree.set(&gk, Some(&child));
    log(&f, &pred, &gk, started(&gk, Some(&child), None));
    f.change(&gk, "e1.txt", Some(b"1"));
    compact(&f, &tree, &pred, &succ, &child);
    f.change(&gk, "e2.txt", Some(b"2"));
    log(&f, &succ, &succ, prompt(&succ, "two"));
    f.change(&child, "e2b.txt", Some(b"2"));
    tree.set(&succ2, None);
    tree.set(&child, Some(&succ2));
    log(&f, &pred, &child, pc(&child, &succ2));
    log(&f, &succ2, &succ2, started(&succ2, None, Some(&succ)));
    log(
        &f,
        &succ2,
        &child,
        LogPayload::Adopted { from: pred.clone() },
    );
    log(&f, &succ2, &succ2, prompt(&succ2, "[Conversation summary]"));
    f.change(&gk, "e3.txt", Some(b"3"));

    let turns: Vec<u32> = f
        .backups
        .turns(&succ)
        .unwrap()
        .iter()
        .map(|t| t.turn)
        .collect();
    assert_eq!(turns, vec![1, 2], "cross-file turns by the write's clock");
    assert_eq!(
        f.backups.revert(&succ2, Some(0)).unwrap().restored,
        vec!["e3.txt"]
    );
    assert_eq!(
        f.backups.revert(&succ, Some(1)).unwrap().restored,
        vec!["e2b.txt"]
    );
    assert_eq!(
        f.backups.revert(&succ, Some(0)).unwrap().restored,
        vec!["e2.txt"]
    );
    assert_eq!(
        f.backups.revert(&pred, Some(0)).unwrap().restored,
        vec!["e1.txt"]
    );
    // A sub-agent target takes its own sub-tree's changes only.
    f.change(&gk, "e4.txt", Some(b"4"));
    f.change(&child, "e5.txt", Some(b"5"));
    let mut r = f.backups.revert(&gk, Some(0)).unwrap().restored;
    r.sort();
    assert_eq!(r, vec!["e4.txt"]);
}

/// Holly's directory moves the child before `ParentChanged` reaches any
/// subscriber, so a write recorded on seeing the event already reads the
/// successor.
#[tokio::test]
async fn hollys_directory_is_moved_before_the_event() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let (pred, succ, child) = (sid("pred"), sid("succ"), sid("child"));
    let spawn =
        |s: &SessionId, parent: Option<&SessionId>, predecessor: Option<&SessionId>| InMsg::Spawn {
            session: s.clone(),
            parent: parent.cloned(),
            predecessor: predecessor.cloned(),
            agent: "general".into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        };
    let ancestry = holly_ancestry(holly.clone());
    holly.send(spawn(&pred, None, None)).await.unwrap();
    holly.send(spawn(&child, Some(&pred), None)).await.unwrap();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("no event")
            .expect("closed");
        if matches!(&ev, OutEvent::SessionStarted { session, .. } if *session == child) {
            break;
        }
    }
    assert_eq!(ancestry(&child), vec![child.clone(), pred.clone()]);
    holly.send(spawn(&succ, None, Some(&pred))).await.unwrap();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("no ParentChanged")
            .expect("closed");
        if let OutEvent::ParentChanged { session, .. } = &ev {
            assert_eq!(session, &child);
            assert_eq!(ancestry(&child)[..2], [child.clone(), succ.clone()]);
            break;
        }
    }
    assert!(ancestry(&sid("nobody")).is_empty());
}

/// A moved child's background script writes on after the child closed: the
/// directory no longer knows it, so the change records no ancestry and goes
/// by each session's latest edge — the successor's, not the predecessor's.
#[tokio::test]
async fn a_closed_childs_background_write_goes_by_its_latest_edge() {
    let f = Fixture::new();
    let (tree, pred, succ, child) = base(&f);
    log(&f, &pred, &child, exec(&child, 2));
    log(&f, &pred, &child, started_output(&child, 3));
    compact(&f, &tree, &pred, &succ, &child);
    tree.remove(&child);
    write_on_task(&f, &child, 2, "bg.txt").await;

    assert!(f
        .backups
        .revert(&pred, Some(0))
        .unwrap()
        .restored
        .is_empty());
    assert_eq!(
        f.backups.revert(&succ, Some(0)).unwrap().restored,
        vec!["bg.txt"]
    );
}

/// The logs lag the move (no `ParentChanged` logged yet), so the busy set
/// read from them misses the moved child — but its recorded change is
/// selected, and its writer must be idle too.
#[test]
fn a_selected_changes_writer_is_busy_checked_even_if_the_logs_lag() {
    let f = Fixture::new();
    let (tree, pred, succ, child) = base(&f);
    tree.set(&succ, None);
    tree.set(&child, Some(&succ));
    log(&f, &succ, &succ, started(&succ, None, Some(&pred)));
    log(
        &f,
        &succ,
        &child,
        LogPayload::Adopted { from: pred.clone() },
    );
    log(&f, &succ, &succ, prompt(&succ, "[Conversation summary]"));
    f.change(&child, "a.txt", Some(b"x"));
    f.backups.fold_for_test(&OutEvent::Status {
        session: child.clone(),
        state: AgentState::Working,
    });
    let err = f.backups.revert(&succ, Some(0)).unwrap_err();
    assert!(err.to_string().contains("`child`"), "{err:#}");
    assert_eq!(f.read("a.txt").unwrap(), b"x");
}

/// An ancestor missing from the directory (a successor not registered yet)
/// ends the chain there: a true prefix, never an empty answer.
#[test]
fn an_ancestry_cut_short_is_a_prefix() {
    let (child, succ) = (sid("child"), sid("succ"));
    let walk = ancestry_walk(&child, |s| (s == &child).then(|| Some(succ.clone())));
    assert_eq!(walk, vec![child.clone(), succ]);
    assert!(
        ancestry_walk(&child, |_| None).is_empty(),
        "unknown session"
    );
}

/// The tree is read before the manifest lock and blob work: a write whose
/// record waits on the lock while a move happens keeps its pre-move tree.
#[test]
fn the_tree_is_read_before_the_record_waits_on_the_lock() {
    let f = Fixture::new();
    let (tree, pred, succ, child) = base(&f);
    let abs = f.path("pre.txt");
    std::fs::write(&abs, "pre").unwrap();
    let guard = lock(&f.backups.inner.manifests);
    let backups = f.backups.clone();
    let (writer, path) = (child.clone(), abs.clone());
    let recording = std::thread::spawn(move || {
        backups.record(&writer, "req", &path, "pre.txt", None, Some(b"pre"));
    });
    std::thread::sleep(Duration::from_millis(50));
    tree.set(&succ, None);
    tree.set(&child, Some(&succ));
    drop(guard);
    recording.join().unwrap();
    compact(&f, &tree, &pred, &succ, &child);

    assert!(f
        .backups
        .revert(&succ, Some(0))
        .unwrap()
        .restored
        .is_empty());
    assert_eq!(
        f.backups.revert(&pred, Some(0)).unwrap().restored,
        vec!["pre.txt"]
    );
}
