use super::*;
use crate::session_store::LogRecord;
use entanglement_core::InMsg;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn started(session: &str, parent: Option<&str>) -> LogRecord {
    LogRecord::new(
        sid(session),
        LogPayload::Out(OutEvent::SessionStarted {
            session: sid(session),
            parent: parent.map(sid),
            predecessor: None,
            agent: "general".into(),
            model: None,
            root: parent.is_none(),
            ts: 0,
            user: None,
        }),
    )
}

fn moved(session: &str, parent: &str) -> LogRecord {
    LogRecord::new(
        sid(session),
        LogPayload::Out(OutEvent::ParentChanged {
            session: sid(session),
            parent: sid(parent),
        }),
    )
}

fn prompt(session: &str, text: &str) -> LogRecord {
    LogRecord::new(
        sid(session),
        LogPayload::In(InMsg::prompt(sid(session), text)),
    )
}

fn pointer(session: &str, from: &str) -> LogRecord {
    LogRecord::new(sid(session), LogPayload::Adopted { from: sid(from) })
}

fn store_with(files: &[(&str, Vec<LogRecord>)]) -> (tempfile::TempDir, SessionStore) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(tmp.path());
    for (root, records) in files {
        for r in records {
            store.append(&sid(root), r).expect("append");
        }
    }
    (tmp, store)
}

fn sessions(records: &[LogRecord]) -> Vec<String> {
    records.iter().map(|r| r.session.0.clone()).collect()
}

/// The moved child's sub-tree (and only it) comes from the predecessor's
/// file, ahead of the adopter's own records; the pointer itself is resolved.
#[test]
fn an_adopted_subtree_is_read_from_its_own_file_first() {
    let (_tmp, store) = store_with(&[
        (
            "pred",
            vec![
                started("pred", None),
                prompt("pred", "p"),
                started("kid", Some("pred")),
                prompt("kid", "k"),
                started("grandkid", Some("kid")),
                started("sibling", Some("pred")),
                moved("kid", "succ"),
                prompt("kid", "after the move"),
            ],
        ),
        ("succ", vec![started("succ", None), pointer("kid", "pred")]),
    ]);
    let tree = store.read_tree(&sid("succ")).expect("read");
    assert_eq!(
        sessions(&tree),
        ["kid", "kid", "grandkid", "kid", "kid", "succ"],
        "{tree:?}"
    );
    assert!(!tree
        .iter()
        .any(|r| matches!(r.payload, LogPayload::Adopted { .. })));
}

/// A second resume: the adopter's file now also holds the child's
/// post-resume records, which must follow (never precede) its history.
#[test]
fn post_resume_records_follow_the_adopted_history() {
    let (_tmp, store) = store_with(&[
        (
            "pred",
            vec![
                started("pred", None),
                started("kid", Some("pred")),
                moved("kid", "succ"),
            ],
        ),
        (
            "succ",
            vec![
                started("succ", None),
                pointer("kid", "pred"),
                started("kid", Some("succ")),
                prompt("kid", "later"),
            ],
        ),
    ]);
    let tree = store.read_tree(&sid("succ")).expect("read");
    assert_eq!(sessions(&tree), ["kid", "kid", "succ", "kid", "kid"]);
}

/// Pointers chain: a child moved twice is found through the middle root.
#[test]
fn pointers_are_followed_transitively() {
    let (_tmp, store) = store_with(&[
        (
            "a",
            vec![
                started("a", None),
                started("kid", Some("a")),
                moved("kid", "b"),
            ],
        ),
        (
            "b",
            vec![
                started("b", None),
                pointer("kid", "a"),
                started("kid", Some("b")),
                moved("kid", "c"),
            ],
        ),
        ("c", vec![started("c", None), pointer("kid", "b")]),
    ]);
    let tree = store.read_tree(&sid("c")).expect("read");
    assert_eq!(sessions(&tree), ["kid", "kid", "kid", "kid", "c"]);
}

/// A pruned source drops the whole adopted sub-tree rather than resuming it
/// from a partial history.
#[test]
fn a_missing_source_drops_the_adopted_subtree() {
    let (_tmp, store) = store_with(&[(
        "succ",
        vec![
            started("succ", None),
            pointer("kid", "gone"),
            started("kid", Some("succ")),
            started("grandkid", Some("kid")),
        ],
    )]);
    let tree = store.read_tree(&sid("succ")).expect("read");
    assert_eq!(sessions(&tree), ["succ"]);
}

/// A gap inside the adopted span is carried over so resume still refuses.
#[test]
fn a_gap_inside_the_adopted_span_is_kept() {
    let gap = LogRecord::new(sid("pred"), LogPayload::Gap { dropped: 2 });
    let (_tmp, store) = store_with(&[
        (
            "pred",
            vec![started("kid", Some("pred")), gap, moved("kid", "succ")],
        ),
        ("succ", vec![started("succ", None), pointer("kid", "pred")]),
    ]);
    let tree = store.read_tree(&sid("succ")).expect("read");
    assert!(crate::session_store::integrity_gap(&tree).is_some());
}

/// A root with no pointers reads exactly as `read` does.
#[test]
fn a_plain_root_reads_unchanged() {
    let (_tmp, store) = store_with(&[("r", vec![started("r", None), prompt("r", "x")])]);
    assert_eq!(
        sessions(&store.read_tree(&sid("r")).unwrap()),
        sessions(&store.read(&sid("r")).unwrap())
    );
}

fn successor_started(session: &str, predecessor: &str) -> LogRecord {
    LogRecord::new(
        sid(session),
        LogPayload::Out(OutEvent::SessionStarted {
            session: sid(session),
            parent: None,
            predecessor: Some(sid(predecessor)),
            agent: "general".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        }),
    )
}

/// P1: a pointer lost to a crash before the successor started is recovered
/// from the immediate predecessor — the child's last move names this tree.
#[test]
fn a_missing_pointer_is_recovered_from_the_immediate_predecessor() {
    let (_tmp, store) = store_with(&[
        (
            "pred",
            vec![
                started("pred", None),
                started("kid", Some("pred")),
                started("stays", Some("pred")),
                moved("kid", "succ"),
                prompt("kid", "after"),
            ],
        ),
        ("succ", vec![successor_started("succ", "pred")]),
    ]);
    let tree = store.read_tree(&sid("succ")).expect("read");
    assert_eq!(sessions(&tree), ["kid", "kid", "kid", "succ"]);

    // With the pointer present too, nothing is adopted twice.
    store.append(&sid("succ"), &pointer("kid", "pred")).unwrap();
    let tree = store.read_tree(&sid("succ")).expect("read");
    assert_eq!(sessions(&tree), ["kid", "kid", "kid", "succ"]);
}

/// A child moved twice is found two files back through its pointer.
#[test]
fn a_child_moved_twice_resolves_through_its_pointer() {
    let (_tmp, store) = store_with(&[
        (
            "a",
            vec![
                started("a", None),
                started("kid", Some("a")),
                moved("kid", "b"),
                moved("kid", "c"),
            ],
        ),
        ("b", vec![successor_started("b", "a"), pointer("kid", "a")]),
        ("c", vec![successor_started("c", "b"), pointer("kid", "a")]),
    ]);
    let tree = store.read_tree(&sid("c")).expect("read");
    assert_eq!(sessions(&tree), ["kid", "kid", "kid", "c"]);
}

/// A pointer cycle (a corrupted store) and a self-pointer terminate and add
/// nothing twice.
#[test]
fn pointer_cycles_and_self_pointers_terminate() {
    let (_tmp, store) = store_with(&[
        (
            "a",
            vec![started("a", None), pointer("x", "b"), pointer("y", "a")],
        ),
        (
            "b",
            vec![
                started("b", None),
                started("x", Some("a")),
                pointer("z", "a"),
            ],
        ),
    ]);
    let tree = store.read_tree(&sid("a")).expect("read");
    assert_eq!(sessions(&tree), ["x", "a"]);
    let tree = store.read_tree(&sid("b")).expect("read");
    assert_eq!(sessions(&tree), ["b", "x"]);
}

/// R2: the lost-pointer fallback reads only the immediate predecessor. A
/// pointer-less move two compactions back is not searched for, and a missing
/// file further down the chain changes nothing.
#[test]
fn the_fallback_looks_one_predecessor_back_only() {
    let (_tmp, store) = store_with(&[
        (
            "b",
            vec![
                successor_started("b", "a"),
                started("kid", Some("b")),
                moved("kid", "d"),
            ],
        ),
        ("c", vec![successor_started("c", "b")]),
        ("d", vec![successor_started("d", "c")]),
    ]);
    // "a" does not exist; "b" is two back from "d" and is never read.
    let tree = store.read_tree(&sid("d")).expect("read");
    assert_eq!(sessions(&tree), ["d"]);
}

/// A child moved twice whose newest pointer was lost: its moves are logged
/// in its spawn-time root, which the predecessor only reaches by pointer, so
/// the fallback reads the predecessor's resolved tree.
#[test]
fn the_fallback_finds_a_twice_moved_child_through_the_predecessors_pointer() {
    let (_tmp, store) = store_with(&[
        (
            "a",
            vec![
                started("a", None),
                started("kid", Some("a")),
                moved("kid", "b"),
                moved("kid", "c"),
                prompt("kid", "later"),
            ],
        ),
        ("b", vec![successor_started("b", "a"), pointer("kid", "a")]),
        ("c", vec![successor_started("c", "b")]),
    ]);
    let tree = store.read_tree(&sid("c")).expect("read");
    assert_eq!(sessions(&tree), ["kid", "kid", "kid", "kid", "c"]);
}
