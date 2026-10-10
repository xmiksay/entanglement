use std::path::PathBuf;
use std::time::Duration;

use entanglement_core::{InMsg, OutEvent, SessionId};

use super::*;
use crate::session_store::{LogPayload, LogRecord};
use crate::tools::Tool;

/// A tempdir holding the session store and a separate working tree.
struct Fixture {
    _tmp: tempfile::TempDir,
    store: SessionStore,
    work: PathBuf,
    backups: FileBackups,
    sid: SessionId,
}

impl Fixture {
    fn new() -> Self {
        Self::with(FileBackups::new)
    }

    fn with(make: impl FnOnce(SessionStore) -> FileBackups) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().join("sessions"));
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        Self {
            backups: make(store.clone()),
            store,
            work,
            _tmp: tmp,
            sid: SessionId::new("root"),
        }
    }

    /// Log a user prompt for `session`, then step the clock so the changes
    /// that follow are strictly later.
    fn prompt(&self, session: &SessionId, text: &str) {
        self.log(LogRecord::new(
            session.clone(),
            LogPayload::In(InMsg::prompt(session.clone(), text)),
        ));
    }

    fn log(&self, record: LogRecord) {
        tick();
        self.store.append(&self.sid, &record).unwrap();
        tick();
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.work.join(rel)
    }

    /// Mimic a tool writing `after` (`None` = deleting) to `rel`.
    fn change(&self, session: &SessionId, rel: &str, after: Option<&[u8]>) {
        let abs = self.path(rel);
        let before = std::fs::read(&abs).ok();
        match after {
            Some(bytes) => {
                std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
                std::fs::write(&abs, bytes).unwrap();
            }
            None => std::fs::remove_file(&abs).unwrap(),
        }
        self.backups
            .record(session, "req", &abs, rel, before.as_deref(), after);
        tick();
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(rel)).ok()
    }
}

/// Changes and prompts are ordered by millisecond timestamps.
fn tick() {
    std::thread::sleep(Duration::from_millis(3));
}

#[test]
fn undo_restores_exact_bytes_and_deletes_a_created_file() {
    let f = Fixture::new();
    std::fs::write(f.path("a.bin"), [0u8, 159, 146, 150]).unwrap();
    f.prompt(&f.sid, "edit things");
    f.change(&f.sid, "a.bin", Some(b"new"));
    f.change(&f.sid, "new/b.txt", Some(b"created"));

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.to_turn, 0);
    assert_eq!(report.restored, vec!["a.bin", "new/b.txt"]);
    assert!(report.skipped.is_empty());
    assert_eq!(f.read("a.bin").unwrap(), vec![0u8, 159, 146, 150]);
    assert_eq!(
        f.read("new/b.txt"),
        None,
        "a file the agent created is removed"
    );
}

#[test]
fn a_file_the_agent_deleted_is_recreated() {
    let f = Fixture::new();
    std::fs::write(f.path("gone.txt"), "keep me").unwrap();
    f.prompt(&f.sid, "clean up");
    f.change(&f.sid, "gone.txt", None);
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["gone.txt"]);
    assert_eq!(f.read("gone.txt").unwrap(), b"keep me");
}

#[test]
fn a_file_changed_since_is_refused_and_reported() {
    let f = Fixture::new();
    std::fs::write(f.path("a.txt"), "v0").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"v1"));
    std::fs::write(f.path("a.txt"), "user edit").unwrap();

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert!(report.restored.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].path, "a.txt");
    assert!(
        report.skipped[0].reason.contains("changed since"),
        "{report:?}"
    );
    assert_eq!(f.read("a.txt").unwrap(), b"user edit", "never clobbered");

    // Still pending: once the user's change is undone, the revert goes through.
    std::fs::write(f.path("a.txt"), "v1").unwrap();
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["a.txt"]);
    assert_eq!(f.read("a.txt").unwrap(), b"v0");
}

#[test]
fn rewind_spans_turns_and_undo_walks_backwards() {
    let f = Fixture::new();
    f.prompt(&f.sid, "one");
    f.change(&f.sid, "a.txt", Some(b"A"));
    f.prompt(&f.sid, "two");
    f.change(&f.sid, "a.txt", Some(b"B"));
    f.prompt(&f.sid, "three");
    f.change(&f.sid, "a.txt", Some(b"C"));
    f.change(&f.sid, "b.txt", Some(b"b"));

    let turns = f.backups.turns(&f.sid).unwrap();
    let rows: Vec<(u32, usize)> = turns.iter().map(|t| (t.turn, t.files)).collect();
    assert_eq!(rows, vec![(1, 1), (2, 1), (3, 2)]);
    assert_eq!(turns[2].prompt, "three");

    let report = f.backups.revert(&f.sid, Some(1)).unwrap();
    assert_eq!(report.to_turn, 1);
    assert_eq!(report.restored, vec!["a.txt", "b.txt"]);
    assert_eq!(f.read("a.txt").unwrap(), b"A");
    assert_eq!(f.read("b.txt"), None);

    // Turns 2–3 are spent; undo now takes turn 1 back.
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.to_turn, 0);
    assert_eq!(f.read("a.txt"), None);
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert!(report.restored.is_empty() && report.skipped.is_empty());
    assert_eq!(
        report.to_turn, 3,
        "nothing left: reported against the last turn"
    );
}

/// A change is placed by its call's `ToolExec` log position, not its own
/// timestamp — here the clock says turn 1, the log says turn 2. A `rhai`
/// binding's `:rhai:` suffix resolves to the outer call.
#[test]
fn a_change_is_placed_by_its_calls_log_position() {
    let f = Fixture::new();
    f.prompt(&f.sid, "one");
    let abs = f.path("a.txt");
    std::fs::write(&abs, "x").unwrap();
    f.backups
        .record(&f.sid, "r1:rhai:write", &abs, "a.txt", None, Some(b"x"));
    f.prompt(&f.sid, "two");
    f.log(LogRecord::new(
        f.sid.clone(),
        LogPayload::Out(OutEvent::ToolExec {
            session: f.sid.clone(),
            seq: 1,
            request_id: "r1".into(),
            tool: "rhai".into(),
            input: "{}".into(),
            agent: "general".into(),
            envelope: None,
        }),
    ));
    let turns: Vec<u32> = f
        .backups
        .turns(&f.sid)
        .unwrap()
        .iter()
        .map(|t| t.turn)
        .collect();
    assert_eq!(turns, vec![2]);
}

#[test]
fn over_cap_changes_are_recorded_as_skipped() {
    let f = Fixture::with(|store| FileBackups::with_limits(store, 8, 12));
    std::fs::write(f.path("big.txt"), "0123456789").unwrap();
    std::fs::write(f.path("s1.txt"), "1234567").unwrap();
    std::fs::write(f.path("s2.txt"), "abcdefg").unwrap();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "big.txt", Some(b"x"));
    f.change(&f.sid, "s1.txt", Some(b"x"));
    f.change(&f.sid, "s2.txt", Some(b"x"));

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["s1.txt"]);
    let reasons: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|s| (s.path.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(reasons[0].0 == "big.txt" && reasons[0].1.contains("per-file backup cap"));
    assert!(reasons[1].0 == "s2.txt" && reasons[1].1.contains("budget"));
}

/// Escape-root writes are covered: a backup keys on the resolved absolute
/// path, so a granted out-of-root write is restored like any other.
#[test]
fn an_out_of_root_write_is_restorable() {
    let f = Fixture::new();
    let outside = f._tmp.path().join("elsewhere/x.txt");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, "orig").unwrap();
    f.prompt(&f.sid, "edit outside");
    std::fs::write(&outside, "changed").unwrap();
    let shown = outside.display().to_string();
    f.backups.record(
        &f.sid,
        "req",
        &outside,
        &shown,
        Some(b"orig"),
        Some(b"changed"),
    );
    tick();
    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec![shown]);
    assert_eq!(std::fs::read(&outside).unwrap(), b"orig");
}

#[test]
fn a_spawned_childs_edits_revert_with_its_parent() {
    let f = Fixture::new();
    let child = SessionId::new("child");
    f.prompt(&f.sid, "delegate");
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
    f.prompt(&child, "do it");
    f.change(&child, "c.txt", Some(b"child wrote"));

    let report = f.backups.revert(&f.sid, None).unwrap();
    assert_eq!(report.restored, vec!["c.txt"]);
    assert_eq!(f.read("c.txt"), None);
}

#[test]
fn backups_go_with_their_session_log() {
    let f = Fixture::new();
    f.prompt(&f.sid, "edit");
    f.change(&f.sid, "a.txt", Some(b"x"));
    let dir = f.store.backups_dir(&f.sid).unwrap();
    assert!(dir.join("manifest.jsonl").exists());

    // An orphan (log gone) is swept by prune; a deleted log takes its backups.
    let orphan = f.store.backups_dir(&SessionId::new("orphan")).unwrap();
    std::fs::create_dir_all(&orphan).unwrap();
    f.store.prune(36500).unwrap();
    assert!(!orphan.exists());
    assert!(dir.exists(), "a live log keeps its backups");
    f.store.delete(&f.sid).unwrap();
    assert!(!dir.exists());
}

#[test]
fn summary_names_restored_skipped_and_the_exec_caveat() {
    let skipped = [RevertSkip {
        path: "b".into(),
        reason: "changed".into(),
    }];
    let s = summary(2, &["a".into()], &skipped);
    assert!(
        s.contains("after turn 2") && s.contains("restored a"),
        "{s}"
    );
    assert!(s.contains("can't restore b: changed"), "{s}");
    assert!(s.contains("not tracked"), "{s}");
    assert_eq!(
        summary(1, &[], &[]),
        "no tracked file edits to revert after turn 1"
    );
}

/// The three file tools each record a change when run for a session.
#[tokio::test]
async fn write_edit_and_apply_patch_are_backed_up() {
    let f = Fixture::new();
    let root = f.work.clone();
    std::fs::write(f.path("p.txt"), "one\ntwo\n").unwrap();
    std::fs::write(f.path("e.txt"), "hello world").unwrap();
    f.prompt(&f.sid, "use the tools");

    let write = crate::host::WriteTool::new(root.clone()).with_backups(f.backups.clone());
    let edit = crate::host::EditTool::new(root.clone()).with_backups(f.backups.clone());
    let patch = crate::host::ApplyPatchTool::new(root).with_backups(f.backups.clone());
    run(&write, &f.sid, r#"{"path":"w.txt","content":"fresh"}"#).await;
    run(
        &edit,
        &f.sid,
        r#"{"path":"e.txt","oldString":"world","newString":"there"}"#,
    )
    .await;
    run(
        &patch,
        &f.sid,
        r#"{"path":"p.txt","patch":"@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"}"#,
    )
    .await;
    tick();

    assert_eq!(f.backups.turns(&f.sid).unwrap()[0].files, 3);
    let report = f.backups.revert(&f.sid, Some(0)).unwrap();
    assert_eq!(report.restored.len(), 3, "{report:?}");
    assert_eq!(f.read("w.txt"), None);
    assert_eq!(f.read("e.txt").unwrap(), b"hello world");
    assert_eq!(f.read("p.txt").unwrap(), b"one\ntwo\n");
}

async fn run(tool: &dyn Tool, session: &SessionId, input: &str) {
    tool.run_for_session(session, "r1", input)
        .await
        .unwrap_or_else(|e| panic!("{} failed: {e:#}", tool.name()));
}
