//! Data-safety cases from the #789 review: repeated request ids, busy
//! sub-trees, atomic restores, lost markers, and undo never getting pinned.

use std::sync::Arc;

use entanglement_core::{AgentState, OutEvent, SessionId};

use super::tests::Fixture;
use super::*;
use crate::session_store::{LogPayload, LogRecord};

fn exec(f: &Fixture, session: &SessionId, seq: u64, request_id: &str) {
    f.log(LogRecord::new(
        session.clone(),
        LogPayload::Out(OutEvent::ToolExec {
            session: session.clone(),
            seq,
            request_id: request_id.into(),
            tool: "edit".into(),
            input: "{}".into(),
            agent: "general".into(),
            envelope: None,
        }),
    ));
}

fn started(f: &Fixture, child: &SessionId) {
    f.log(LogRecord::new(
        child.clone(),
        LogPayload::Out(OutEvent::SessionStarted {
            session: child.clone(),
            parent: Some(f.sid.clone()),
            predecessor: None,
            agent: "general".into(),
            model: None,
            root: false,
            ts: 0,
            user: None,
        }),
    ));
    f.backups.note_root(child.clone(), f.sid.clone());
}

/// Gemini call ids are `name#ordinal`, so `edit#0` recurs every turn and in
/// children. Each change must still land in its own turn: an undo takes back
/// turn 2 (root + child) and leaves turn 1 alone.
#[tokio::test]
async fn repeated_request_ids_stay_in_their_own_turn() {
    let f = Fixture::new();
    let child = SessionId::new("child");
    f.prompt(&f.sid, "one");
    exec(&f, &f.sid, 3, "edit#0");
    with_exec_seq(3, async { f.change(&f.sid, "a.txt", Some(b"turn one")) }).await;
    f.prompt(&f.sid, "two");
    exec(&f, &f.sid, 9, "edit#0");
    with_exec_seq(9, async { f.change(&f.sid, "b.txt", Some(b"turn two")) }).await;
    started(&f, &child);
    f.prompt(&child, "help");
    exec(&f, &child, 2, "edit#0");
    with_exec_seq(2, async { f.change(&child, "c.txt", Some(b"child")) }).await;

    let rows: Vec<(u32, usize)> = f
        .backups
        .turns(&f.sid)
        .unwrap()
        .iter()
        .map(|t| (t.turn, t.files))
        .collect();
    assert_eq!(rows, vec![(1, 1), (2, 2)]);
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.to_turn, 1);
    assert_eq!(report.restored, vec!["b.txt", "c.txt"]);
    assert_eq!(f.read("a.txt").unwrap(), b"turn one", "turn 1 untouched");
}

/// Off the call task (a detached script) a change has no seq: it falls back
/// to the latest `(session, request_id)` call logged no later than itself.
#[test]
fn without_a_seq_repeated_ids_resolve_to_the_latest_earlier_call() {
    let f = Fixture::new();
    f.prompt(&f.sid, "one");
    exec(&f, &f.sid, 1, "edit#0");
    f.change(&f.sid, "a.txt", Some(b"1"));
    f.prompt(&f.sid, "two");
    exec(&f, &f.sid, 5, "edit#0");
    f.change(&f.sid, "b.txt", Some(b"2"));
    let turns: Vec<u32> = f
        .backups
        .turns(&f.sid)
        .unwrap()
        .iter()
        .map(|t| t.turn)
        .collect();
    assert_eq!(turns, vec![1, 2]);
}

/// A revert refuses while any session of the sub-tree may still write — a
/// child mid-turn, or detached work the probe reports.
#[test]
fn a_busy_sub_tree_refuses_the_revert() {
    let f = Fixture::new();
    let child = SessionId::new("child");
    f.prompt(&f.sid, "one");
    f.change(&f.sid, "a.txt", Some(b"x"));
    started(&f, &child);

    f.backups.fold_for_test(&OutEvent::Status {
        session: child.clone(),
        state: AgentState::Working,
    });
    let err = f.backups.revert(&f.sid, None).unwrap_err();
    assert!(err.to_string().contains("still working"), "{err:#}");
    assert_eq!(f.read("a.txt").unwrap(), b"x", "nothing touched");

    f.backups.fold_for_test(&OutEvent::Status {
        session: child,
        state: AgentState::Done,
    });
    f.backups
        .set_busy_probe(Arc::new(|s: &SessionId| s.0 == "root"));
    assert!(
        f.backups.revert(&f.sid, None).is_err(),
        "detached work refuses"
    );
}

/// The restored file keeps its permission bits and no temp file is left.
#[cfg(unix)]
#[test]
fn restore_is_atomic_and_keeps_the_mode() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let script = f.path("run.sh");
    std::fs::write(&script, "#!/bin/sh\necho old\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "run.sh", Some(b"#!/bin/sh\necho new\n"));

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["run.sh"]);
    assert_eq!(f.read("run.sh").unwrap(), b"#!/bin/sh\necho old\n");
    let mode = std::fs::metadata(&script).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    let leftovers: Vec<_> = std::fs::read_dir(&f.work)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// A file already back at its `before` (an earlier revert whose marker was
/// lost) counts as restored and is consumed, so undo moves on.
#[test]
fn a_file_already_restored_is_consumed() {
    let f = Fixture::new();
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    f.prompt(&f.sid, "one");
    f.change(&f.sid, "a.txt", Some(b"v1"));
    std::fs::write(f.path("a.txt"), "v0").unwrap();

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    let again = f.backups.revert(&f.sid, None).unwrap();
    assert!(again.restored.is_empty() && again.skipped.is_empty());
}

/// A marker that cannot be written never turns a done revert into an error.
#[cfg(unix)]
#[test]
fn a_lost_marker_still_reports_the_revert() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    f.prompt(&f.sid, "one");
    f.change(&f.sid, "a.txt", Some(b"v1"));
    let manifest = f.store.backups_dir(&f.sid).unwrap().join("manifest.jsonl");
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o444)).unwrap();

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    assert_eq!(f.read("a.txt").unwrap(), b"v0");
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// Neither a never-restorable change (no backup) nor a conflicted one pins
/// `/undo`: the no-backup one is reported once and consumed, the conflicted
/// one stays revertable but undo steps past it to the earlier turn.
#[test]
fn unrestorable_changes_do_not_pin_undo() {
    let f = Fixture::with(|store| FileBackups::with_limits(store, 4, 1024));
    std::fs::write(f.path("big.txt"), "0123456789").unwrap();
    f.prompt(&f.sid, "one");
    f.change(&f.sid, "a.txt", Some(b"a"));
    f.prompt(&f.sid, "two");
    f.change(&f.sid, "c.txt", Some(b"c"));
    f.prompt(&f.sid, "three");
    f.change(&f.sid, "big.txt", Some(b"x"));

    let r = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!((r.to_turn, r.skipped.len()), (2, 1), "{r:?}");
    assert!(r.skipped[0].reason.contains("no backup"));

    std::fs::write(f.path("c.txt"), "user").unwrap();
    let r = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(r.to_turn, 1, "big.txt no longer pins the undo");
    assert!(r.skipped[0].reason.contains("changed since"));

    let r = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(r.to_turn, 0, "the conflicted c.txt does not pin it either");
    assert_eq!(r.restored, vec!["a.txt"]);
    assert_eq!(f.read("c.txt").unwrap(), b"user", "never clobbered");
}

/// The restore's temp file is created exclusively and never through a
/// symlink: one planted at the temp path pointing outside the tree errors
/// instead of writing there.
#[cfg(unix)]
#[test]
fn a_planted_symlink_is_never_written_through() {
    let f = Fixture::new();
    let outside = f._tmp.path().join("outside.txt");
    std::fs::write(&outside, "untouched").unwrap();
    let planted = f.path(".er-planted.tmp");
    std::os::unix::fs::symlink(&outside, &planted).unwrap();

    assert!(super::restore::create_exclusive(&planted).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");

    // And a real restore beside the plant goes through its own fresh name.
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"v1"));
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
}

/// A directory swapped for a symlink after the agent's write never steers
/// the restore outside: even a file there whose bytes match the recorded
/// `after` is left alone, and the skip stays pending.
#[cfg(unix)]
#[test]
fn a_directory_swapped_for_a_symlink_is_refused() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.path("sub")).unwrap();
    std::fs::write(f.path("sub/kept.txt"), "orig").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "sub/new.txt", Some(b"x"));
    f.change(&f.sid, "sub/kept.txt", Some(b"x"));

    let outside = f._tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("new.txt"), "x").unwrap();
    std::fs::write(outside.join("kept.txt"), "x").unwrap();
    std::fs::remove_dir_all(f.path("sub")).unwrap();
    std::os::unix::fs::symlink(&outside, f.path("sub")).unwrap();

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert!(report.restored.is_empty(), "{report:?}");
    assert_eq!(report.skipped.len(), 2);
    assert!(report
        .skipped
        .iter()
        .all(|s| s.reason.contains("directory moved")));
    assert_eq!(std::fs::read(outside.join("new.txt")).unwrap(), b"x");
    assert_eq!(std::fs::read(outside.join("kept.txt")).unwrap(), b"x");
}

/// A filename near `NAME_MAX` still restores: the temp name is short.
#[test]
fn a_long_filename_restores() {
    let f = Fixture::new();
    let name = format!("{}.txt", "n".repeat(240));
    std::fs::write(f.path(&name), "v0").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, &name, Some(b"v1"));
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec![name.clone()]);
    assert_eq!(f.read(&name).unwrap(), b"v0");
}

/// A lagged event stream may have hidden a child starting a turn: every
/// known session counts as busy until it reports a fresh `Status` — except
/// the revert's own (idle) target.
#[test]
fn a_lagged_tracker_refuses_until_sessions_report_again() {
    let f = Fixture::new();
    let child = SessionId::new("child");
    f.prompt(&f.sid, "one");
    f.change(&f.sid, "a.txt", Some(b"x"));
    started(&f, &child);
    f.backups.note_root(f.sid.clone(), f.sid.clone());

    f.backups.poison_for_test();
    assert!(f.backups.revert(&f.sid, None).is_err(), "child unknown");
    f.backups.fold_for_test(&OutEvent::Status {
        session: child,
        state: AgentState::Idle,
    });
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(
        report.restored,
        vec!["a.txt"],
        "the target itself never blocks"
    );
}
