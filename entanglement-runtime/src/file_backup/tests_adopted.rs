//! Reverting a compaction successor reaches the sub-agents it adopted
//! (#870, ADR-0234): their backups stay under the root they started in.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::{AgentState, InMsg, OutEvent, SessionId};

use super::tests::{tick, Fixture};
use super::tracker::ancestry_walk;
use super::{with_exec_seq, without_exec_seq, FileBackups};
use crate::session_store::{LogPayload, LogRecord};

/// A stand-in for Holly's session directory: who is under whom right now.
#[derive(Clone, Default)]
pub(super) struct LiveTree(Arc<Mutex<HashMap<SessionId, Option<SessionId>>>>);

impl LiveTree {
    pub(super) fn install(backups: &FileBackups) -> Self {
        let tree = Self::default();
        let probe = tree.clone();
        backups.set_ancestry_probe(Arc::new(move |s: &SessionId| probe.ancestry(s)));
        tree
    }

    pub(super) fn set(&self, session: &SessionId, parent: Option<&SessionId>) {
        self.0
            .lock()
            .unwrap()
            .insert(session.clone(), parent.cloned());
    }

    /// The session leaves the directory (closed or hibernated).
    pub(super) fn remove(&self, session: &SessionId) {
        self.0.lock().unwrap().remove(session);
    }

    fn ancestry(&self, session: &SessionId) -> Vec<SessionId> {
        let map = self.0.lock().unwrap().clone();
        ancestry_walk(session, |s| map.get(s).cloned())
    }
}

pub(super) fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

/// Append `payload` by `session` to root file `file`, between clock ticks.
pub(super) fn log(f: &Fixture, file: &SessionId, session: &SessionId, payload: LogPayload) {
    tick();
    f.store
        .append(file, &LogRecord::new(session.clone(), payload))
        .unwrap();
    tick();
}

pub(super) fn started(
    session: &SessionId,
    parent: Option<&SessionId>,
    predecessor: Option<&SessionId>,
) -> LogPayload {
    LogPayload::Out(OutEvent::SessionStarted {
        session: session.clone(),
        parent: parent.cloned(),
        predecessor: predecessor.cloned(),
        agent: "general".into(),
        model: None,
        root: parent.is_none(),
        ts: 0,
        user: None,
    })
}

pub(super) fn prompt(session: &SessionId, text: &str) -> LogPayload {
    LogPayload::In(InMsg::prompt(session.clone(), text))
}

/// The `ToolExec` a following `Fixture::change` (request id `req`) is placed by.
pub(super) fn exec(session: &SessionId, seq: u64) -> LogPayload {
    LogPayload::Out(OutEvent::ToolExec {
        session: session.clone(),
        seq,
        request_id: "req".into(),
        tool: "write".into(),
        input: "{}".into(),
        agent: String::new(),
        envelope: None,
        workdir: None,
    })
}

/// Root `pred` spawned `child` (logging and backing up under `pred`'s
/// file), then compacted into root `succ`, which adopted `child` through a
/// pointer and got its seed prompt. Before the move, `child` wrote `pre.txt`
/// and `pred` wrote `own.txt`.
pub(super) struct Moved {
    pub(super) f: Fixture,
    pub(super) pred: SessionId,
    pub(super) succ: SessionId,
    pub(super) child: SessionId,
}

pub(super) fn moved() -> Moved {
    moved_with(true)
}

/// `pointer: false` is a crash that lost the `Adopted` pointer: the
/// successor still finds the child through its predecessor's log.
fn moved_with(pointer: bool) -> Moved {
    let f = Fixture::new();
    let tree = LiveTree::install(&f.backups);
    let (pred, succ, child) = (sid("pred"), sid("succ"), sid("child"));
    tree.set(&pred, None);
    tree.set(&child, Some(&pred));
    f.backups.note_root(pred.clone(), pred.clone());
    f.backups.note_root(child.clone(), pred.clone());
    f.backups.note_root(succ.clone(), succ.clone());

    log(&f, &pred, &pred, started(&pred, None, None));
    log(&f, &pred, &pred, prompt(&pred, "delegate"));
    log(&f, &pred, &pred, exec(&pred, 1));
    f.change(&pred, "own.txt", Some(b"pred"));
    log(&f, &pred, &child, started(&child, Some(&pred), None));
    log(&f, &pred, &child, prompt(&child, "task"));
    log(&f, &pred, &child, exec(&child, 1));
    f.change(&child, "pre.txt", Some(b"before the move"));

    let moved = LogPayload::Out(OutEvent::ParentChanged {
        session: child.clone(),
        parent: succ.clone(),
    });
    // The directory moves the child first; the logs follow.
    tree.set(&succ, None);
    tree.set(&child, Some(&succ));
    log(&f, &pred, &child, moved);
    log(&f, &succ, &succ, started(&succ, None, Some(&pred)));
    if pointer {
        let adopted = LogPayload::Adopted { from: pred.clone() };
        log(&f, &succ, &child, adopted);
    }
    log(&f, &succ, &succ, prompt(&succ, "[Conversation summary]"));
    Moved {
        f,
        pred,
        succ,
        child,
    }
}

#[test]
fn a_successor_rewind_restores_a_moved_childs_later_write() {
    let m = moved();
    let f = &m.f;
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    log(f, &m.pred, &m.child, exec(&m.child, 2));
    f.change(&m.child, "a.txt", Some(b"child, after the move"));

    let turns = f.backups.turns(&m.succ).unwrap();
    assert_eq!(
        turns.len(),
        1,
        "the predecessor-era write is not offered: {turns:?}"
    );
    assert_eq!((turns[0].turn, turns[0].files), (1, 1));

    let report = f.backups.revert(&m.succ, Some(0)).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    assert_eq!(f.read("a.txt").unwrap(), b"v0");
    assert_eq!(
        f.read("pre.txt").unwrap(),
        b"before the move",
        "a write from before the successor existed stays"
    );
    assert_eq!(
        f.read("own.txt").unwrap(),
        b"pred",
        "the predecessor's own write stays"
    );

    // The marker landed in the child's manifest: nothing is left to revert.
    assert!(f.backups.turns(&m.succ).unwrap().is_empty());
    let again = f.backups.revert(&m.succ, Some(0)).unwrap();
    assert!(
        again.restored.is_empty() && again.skipped.is_empty(),
        "{again:?}"
    );
}

#[test]
fn a_rewind_orders_one_files_changes_across_both_manifests() {
    let m = moved();
    let f = &m.f;
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    log(f, &m.succ, &m.succ, exec(&m.succ, 1));
    f.change(&m.succ, "a.txt", Some(b"succ"));
    log(f, &m.pred, &m.child, exec(&m.child, 2));
    f.change(&m.child, "a.txt", Some(b"child"));
    log(f, &m.succ, &m.succ, prompt(&m.succ, "more"));
    log(f, &m.pred, &m.child, exec(&m.child, 3));
    f.change(&m.child, "b.txt", Some(b"turn two"));

    // Undo takes turn 2 only.
    let report = f.backups.revert(&m.succ, None).unwrap();
    assert_eq!(
        (report.to_turn, report.restored.clone()),
        (1, vec!["b.txt".to_string()])
    );
    assert_eq!(f.read("b.txt"), None);

    // The succ write came first: its prior bytes win, and the child's
    // content is what the conflict check expects on disk.
    let report = f.backups.revert(&m.succ, Some(0)).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    assert_eq!(f.read("a.txt").unwrap(), b"v0");
}

#[test]
fn a_cold_resumed_childs_writes_in_both_roots_are_reverted() {
    let m = moved();
    let f = &m.f;
    log(f, &m.pred, &m.child, exec(&m.child, 2));
    f.change(&m.child, "live.txt", Some(b"before the restart"));

    // A restart: the child re-announces itself under the successor and now
    // logs, and backs up, into the successor's root.
    let backups = super::FileBackups::new(f.store.clone());
    backups.note_root(m.succ.clone(), m.succ.clone());
    backups.note_root(m.child.clone(), m.succ.clone());
    log(f, &m.succ, &m.child, started(&m.child, Some(&m.succ), None));
    log(f, &m.succ, &m.child, exec(&m.child, 3));
    std::fs::write(f.path("resumed.txt"), "after").unwrap();
    backups.record(
        &m.child,
        "req",
        &f.path("resumed.txt"),
        "resumed.txt",
        None,
        Some(b"after"),
    );

    let report = backups.revert(&m.succ, Some(0)).unwrap();
    let mut restored = report.restored.clone();
    restored.sort();
    assert_eq!(restored, vec!["live.txt", "resumed.txt"]);
    assert_eq!(f.read("live.txt"), None);
    assert_eq!(f.read("resumed.txt"), None);
}

#[test]
fn a_busy_moved_child_refuses_the_successors_revert() {
    let m = moved();
    let f = &m.f;
    log(f, &m.pred, &m.child, exec(&m.child, 2));
    f.change(&m.child, "a.txt", Some(b"x"));
    f.backups.fold_for_test(&OutEvent::Status {
        session: m.child.clone(),
        state: AgentState::Working,
    });
    let err = f.backups.revert(&m.succ, Some(0)).unwrap_err();
    assert!(err.to_string().contains("`child`"), "{err:#}");
    assert_eq!(f.read("a.txt").unwrap(), b"x");
}

#[test]
fn a_root_without_adoptions_never_reaches_another_roots_backups() {
    let m = moved();
    let f = &m.f;
    // `other` was never compacted into: no pointer, no move names it.
    let other = sid("other");
    f.backups.note_root(other.clone(), other.clone());
    log(f, &other, &other, started(&other, None, None));
    log(f, &other, &other, prompt(&other, "unrelated"));
    log(f, &m.pred, &m.child, exec(&m.child, 2));
    f.change(&m.child, "a.txt", Some(b"child"));
    log(f, &other, &other, exec(&other, 1));
    f.change(&other, "o.txt", Some(b"other"));

    let report = f.backups.revert(&other, Some(0)).unwrap();
    assert_eq!(report.restored, vec!["o.txt"]);
    assert_eq!(
        f.read("a.txt").unwrap(),
        b"child",
        "the moved child is not other's"
    );
}

#[test]
fn a_lost_pointer_still_reaches_the_moved_child() {
    let m = moved_with(false);
    let f = &m.f;
    log(f, &m.pred, &m.child, exec(&m.child, 2));
    f.change(&m.child, "a.txt", Some(b"child"));
    let report = f.backups.revert(&m.succ, Some(0)).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    assert_eq!(f.read("a.txt"), None);
}

/// A switched background script's change (ADR-0241) keeps the launch's
/// request id, which a provider that repeats ids (Gemini's `name#ordinal`)
/// can put on an unrelated call in the successor's log: the change is placed
/// by its wall clock, never into that call's turn (ADR-0243).
#[tokio::test]
async fn an_adopted_change_is_placed_by_its_clock_not_a_reused_request_id() {
    let f = Fixture::new();
    let s = f.sid.clone();
    f.prompt(&s, "one");
    // The same request id `req` the change below is recorded with.
    log(&f, &s, &s, exec(&s, 1));
    f.prompt(&s, "two");
    let adopted = without_exec_seq(async { f.change(&s, "f.txt", Some(b"x")) });
    with_exec_seq(9, adopted).await;
    let turns = f.backups.turns(&s).unwrap();
    assert_eq!(turns.iter().map(|t| t.turn).collect::<Vec<_>>(), vec![2]);
}
