//! Tree-membership edge cases from the #870 reviews (ADR-0236): grandchildren
//! across a move, cross-file turns, an unreadable adopted manifest, the merge
//! order across and within manifests, edges across files, and a move inside
//! one tree.

use entanglement_core::{AgentState, OutEvent, SessionId};

use super::tests::Fixture;
use super::tests_adopted::{exec, log, moved, prompt, sid, started, LiveTree};
use crate::session_store::LogPayload;

fn pc(child: &SessionId, parent: &SessionId) -> LogPayload {
    LogPayload::Out(OutEvent::ParentChanged {
        session: child.clone(),
        parent: parent.clone(),
    })
}

/// `pred` spawned `child`; both log (and back up) under `pred`'s file.
fn base(f: &Fixture, pred: &str) -> (SessionId, SessionId, SessionId) {
    let (pred, succ, child) = (sid(pred), sid("succ"), sid("child"));
    f.backups.note_root(pred.clone(), pred.clone());
    f.backups.note_root(child.clone(), pred.clone());
    f.backups.note_root(succ.clone(), succ.clone());
    log(f, &pred, &pred, started(&pred, None, None));
    log(f, &pred, &pred, prompt(&pred, "delegate"));
    log(f, &pred, &child, started(&child, Some(&pred), None));
    log(f, &pred, &child, prompt(&child, "task"));
    (pred, succ, child)
}

fn succ_starts(f: &Fixture, pred: &SessionId, succ: &SessionId, from: &SessionId, kid: &SessionId) {
    log(f, succ, succ, started(succ, None, Some(pred)));
    log(f, succ, kid, LogPayload::Adopted { from: from.clone() });
    log(f, succ, succ, prompt(succ, "[Conversation summary]"));
}

#[test]
fn rv_grandchild_before_and_after_move() {
    let f = Fixture::new();
    let tree = LiveTree::install(&f.backups);
    let (pred, succ, child) = base(&f, "pred");
    let (gk, gk2) = (sid("gk"), sid("gk2"));
    tree.set(&pred, None);
    tree.set(&child, Some(&pred));
    tree.set(&gk, Some(&child));
    f.backups.note_root(gk.clone(), pred.clone());
    f.backups.note_root(gk2.clone(), pred.clone());
    log(&f, &pred, &gk, started(&gk, Some(&child), None));
    log(&f, &pred, &gk, exec(&gk, 1));
    f.change(&gk, "gpre.txt", Some(b"x"));
    tree.set(&succ, None);
    tree.set(&child, Some(&succ));
    log(&f, &pred, &child, pc(&child, &succ));
    succ_starts(&f, &pred, &succ, &pred, &child);
    log(&f, &pred, &gk, exec(&gk, 2));
    f.change(&gk, "gpost.txt", Some(b"x"));
    tree.set(&gk2, Some(&child));
    log(&f, &pred, &gk2, started(&gk2, Some(&child), None));
    log(&f, &pred, &gk2, exec(&gk2, 1));
    f.change(&gk2, "g2.txt", Some(b"x"));
    let status = |state| OutEvent::Status {
        session: gk2.clone(),
        state,
    };
    f.backups.fold_for_test(&status(AgentState::Working));
    assert!(f.backups.revert(&succ, Some(0)).is_err(), "busy grandchild");
    assert!(
        f.backups.revert(&pred, Some(0)).is_ok(),
        "pred doesn't wait"
    );
    f.backups.fold_for_test(&status(AgentState::Idle));
    let mut r = f.backups.revert(&succ, Some(0)).unwrap().restored;
    r.sort();
    assert_eq!(r, vec!["g2.txt", "gpost.txt"]);
}

#[test]
fn rv_turns_of_moved_child_vs_successor_prompts() {
    let f = Fixture::new();
    let (pred, succ, child) = base(&f, "pred");
    log(&f, &pred, &child, pc(&child, &succ));
    succ_starts(&f, &pred, &succ, &pred, &child);
    log(&f, &pred, &child, exec(&child, 1));
    f.change(&child, "t1.txt", Some(b"x"));
    log(&f, &succ, &succ, prompt(&succ, "p2"));
    log(&f, &pred, &child, exec(&child, 2));
    f.change(&child, "t2.txt", Some(b"x"));
    let t: Vec<u32> = f
        .backups
        .turns(&succ)
        .unwrap()
        .iter()
        .map(|t| t.turn)
        .collect();
    assert_eq!(t, vec![1, 2]);
    assert_eq!(
        f.backups.revert(&succ, Some(1)).unwrap().restored,
        vec!["t2.txt"]
    );
    assert_eq!(
        f.backups.revert(&succ, None).unwrap().restored,
        vec!["t1.txt"]
    );
}

/// An adopted manifest that cannot be read (here a directory, a real I/O
/// error) is skipped; the root's own changes still revert.
#[test]
fn an_unreadable_adopted_manifest_is_skipped() {
    let f = Fixture::new();
    let (pred, succ, child) = base(&f, "pred");
    log(&f, &pred, &child, pc(&child, &succ));
    succ_starts(&f, &pred, &succ, &pred, &child);
    log(&f, &succ, &succ, exec(&succ, 1));
    f.change(&succ, "s.txt", Some(b"x"));
    let manifest = f
        .store
        .backups_dir(&pred)
        .unwrap()
        .join(super::manifest::MANIFEST);
    std::fs::create_dir_all(&manifest).unwrap();
    let r = f.backups.revert(&succ, Some(0)).unwrap();
    assert_eq!(r.restored, vec!["s.txt"], "{r:?}");

    // The root's own manifest unreadable is still an error.
    let own = f
        .store
        .backups_dir(&succ)
        .unwrap()
        .join(super::manifest::MANIFEST);
    std::fs::remove_file(&own).unwrap();
    std::fs::create_dir_all(&own).unwrap();
    assert!(f.backups.revert(&succ, Some(0)).is_err());
}

/// The clock steps back between two changes of one file in one manifest:
/// record order, not the wall clock, decides which change is first.
#[test]
fn one_manifests_order_survives_a_clock_step_back() {
    let f = Fixture::new();
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"v1"));
    f.change(&f.sid, "a.txt", Some(b"v2"));
    let manifest = f
        .store
        .backups_dir(&f.sid)
        .unwrap()
        .join(super::manifest::MANIFEST);
    let text = std::fs::read_to_string(&manifest).unwrap();
    let mut lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let first = lines[0]["ts"].as_u64().unwrap();
    lines[1]["ts"] = (first - 1).into();
    let out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    std::fs::write(&manifest, out.join("\n") + "\n").unwrap();

    let r = f.backups.revert(&f.sid, Some(0)).unwrap();
    assert_eq!(r.restored, vec!["a.txt"], "{r:?}");
    assert_eq!(f.read("a.txt").unwrap(), b"v0");
}

/// A child's edges sit in two files whose name order is the reverse of
/// their time order (`zpred` holds the older move): merged by time, its
/// latest parent is `succ2`, so its post-resume write is `succ2`'s.
#[test]
fn edges_across_files_are_merged_by_time_not_file_order() {
    let f = Fixture::new();
    let (pred, succ, child) = base(&f, "zpred");
    let succ2 = sid("succ2");
    f.backups.note_root(succ2.clone(), succ2.clone());
    log(&f, &pred, &child, pc(&child, &succ));
    succ_starts(&f, &pred, &succ, &pred, &child);
    // A cold resume: the child re-announces itself under `succ`'s root.
    f.backups.note_root(child.clone(), succ.clone());
    log(&f, &succ, &child, started(&child, Some(&succ), None));
    log(&f, &succ, &child, pc(&child, &succ2));
    succ_starts(&f, &succ, &succ2, &succ, &child);
    log(&f, &succ, &child, exec(&child, 1));
    f.change(&child, "late.txt", Some(b"x"));

    f.backups.fold_for_test(&OutEvent::Status {
        session: child.clone(),
        state: AgentState::Working,
    });
    assert!(
        f.backups.revert(&succ2, Some(0)).is_err(),
        "succ2 waits on it"
    );
    f.backups.fold_for_test(&OutEvent::Status {
        session: child,
        state: AgentState::Idle,
    });
    assert_eq!(
        f.backups.revert(&succ2, Some(0)).unwrap().restored,
        vec!["late.txt"]
    );
}

/// The same file changed by the child first (its manifest) and the successor
/// after (another): the merge must put the child's change first, so the
/// child's `before` is restored and the successor's `after` is what the
/// conflict check expects.
#[test]
fn a_rewind_merges_the_child_manifest_first_when_it_wrote_first() {
    let m = moved();
    let f = &m.f;
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    log(f, &m.pred, &m.child, exec(&m.child, 2));
    f.change(&m.child, "a.txt", Some(b"child"));
    log(f, &m.succ, &m.succ, exec(&m.succ, 1));
    f.change(&m.succ, "a.txt", Some(b"succ"));

    let report = f.backups.revert(&m.succ, Some(0)).unwrap();
    assert_eq!(report.restored, vec!["a.txt"], "{report:?}");
    assert!(report.skipped.is_empty());
    assert_eq!(f.read("a.txt").unwrap(), b"v0");
}

/// A sub-agent's compaction moves its children to its successor, both under
/// the same root: the grandchild's writes from before and after the move
/// both stay in the root's tree.
#[test]
fn a_move_within_the_tree_keeps_every_write() {
    let f = Fixture::new();
    let (root, sub, sub2, kid) = (sid("root"), sid("sub"), sid("sub2"), sid("kid"));
    for s in [&root, &sub, &sub2, &kid] {
        f.backups.note_root(s.clone(), root.clone());
    }
    log(&f, &root, &root, started(&root, None, None));
    log(&f, &root, &root, prompt(&root, "go"));
    log(&f, &root, &sub, started(&sub, Some(&root), None));
    log(&f, &root, &kid, started(&kid, Some(&sub), None));
    log(&f, &root, &kid, exec(&kid, 1));
    f.change(&kid, "pre.txt", Some(b"pre"));
    log(&f, &root, &kid, pc(&kid, &sub2));
    log(&f, &root, &sub2, started(&sub2, Some(&root), Some(&sub)));
    log(&f, &root, &kid, exec(&kid, 2));
    f.change(&kid, "post.txt", Some(b"post"));

    let report = f.backups.revert(&root, Some(0)).unwrap();
    let mut restored = report.restored.clone();
    restored.sort();
    assert_eq!(restored, vec!["post.txt", "pre.txt"]);
}
