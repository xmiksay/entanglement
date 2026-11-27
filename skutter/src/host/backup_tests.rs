//! The file tools' per-write backups (ADR-0212): each records a revertable
//! change, and stamps it *before* it writes (ADR-0236 §2), so the ancestry
//! probe still sees the file's pre-write state.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{InMsg, SessionId};

use super::{ApplyPatchTool, EditTool, WriteTool};
use crate::file_backup::FileBackups;
use crate::session_store::SessionStore;
use crate::tools::Tool;
use entanglement_runtime::session_store::{LogPayload, LogRecord};

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
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::at(tmp.path().join("sessions"));
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        Self {
            backups: FileBackups::new(store.clone()),
            store,
            work,
            _tmp: tmp,
            sid: SessionId::new("root"),
        }
    }

    /// Log a user prompt, then step the clock so the changes that follow
    /// are strictly later (changes are ordered by millisecond timestamps).
    fn prompt(&self, text: &str) {
        tick();
        let record = LogRecord::new(
            self.sid.clone(),
            LogPayload::In(InMsg::prompt(self.sid.clone(), text)),
        );
        self.store.append(&self.sid, &record).unwrap();
        tick();
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.work.join(rel)
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(rel)).ok()
    }
}

fn tick() {
    std::thread::sleep(Duration::from_millis(3));
}

async fn run(tool: &dyn Tool, f: &Fixture, input: serde_json::Value) {
    tool.run_for_session(&f.sid, "req", &input.to_string())
        .await
        .unwrap_or_else(|e| panic!("{} failed: {e:#}", tool.name()));
}

/// The three file tools each record a change when run for a session.
#[tokio::test]
async fn write_edit_and_apply_patch_are_backed_up() {
    let f = Fixture::new();
    let root = f.work.clone();
    std::fs::write(f.path("p.txt"), "one\ntwo\n").unwrap();
    std::fs::write(f.path("e.txt"), "hello world").unwrap();
    f.prompt("use the tools");

    let write = WriteTool::new(root.clone()).with_backups(f.backups.clone());
    let edit = EditTool::new(root.clone()).with_backups(f.backups.clone());
    let patch = ApplyPatchTool::new(root).with_backups(f.backups.clone());
    let fresh = serde_json::json!({"path": "w.txt", "content": "fresh"});
    run(&write, &f, fresh).await;
    let there = serde_json::json!({"path": "e.txt", "oldString": "world", "newString": "there"});
    run(&edit, &f, there).await;
    let hunk = "@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n";
    run(
        &patch,
        &f,
        serde_json::json!({"path": "p.txt", "patch": hunk}),
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

/// Install a probe that snapshots `target` each time it is asked; returns
/// the snapshots (`None` = no file).
fn snapshotting_probe(f: &Fixture, target: PathBuf) -> Arc<Mutex<Vec<Option<Vec<u8>>>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    f.backups.set_ancestry_probe(Arc::new(move |s: &SessionId| {
        log.lock().unwrap().push(std::fs::read(&target).ok());
        vec![s.clone()]
    }));
    seen
}

#[tokio::test]
async fn every_file_tool_stamps_before_it_writes() {
    let f = Fixture::new();
    let target = f.path("a.txt");
    let seen = snapshotting_probe(&f, target.clone());
    let root = f.work.clone();

    let write = WriteTool::new(root.clone()).with_backups(f.backups.clone());
    run(
        &write,
        &f,
        serde_json::json!({"path": "a.txt", "content": "one\n"}),
    )
    .await;
    std::fs::remove_file(&target).unwrap();

    let edit = EditTool::new(root.clone()).with_backups(f.backups.clone());
    let create = serde_json::json!({"path": "a.txt", "oldString": "", "newString": "two\n"});
    run(&edit, &f, create).await;
    let replace = serde_json::json!({"path": "a.txt", "oldString": "two", "newString": "three"});
    run(&edit, &f, replace).await;

    let patch = ApplyPatchTool::new(root).with_backups(f.backups.clone());
    let hunk = "@@ -1,1 +1,1 @@\n-three\n+four\n";
    run(
        &patch,
        &f,
        serde_json::json!({"path": "a.txt", "patch": hunk}),
    )
    .await;

    let seen = seen.lock().unwrap().clone();
    let expected: Vec<Option<Vec<u8>>> = vec![
        None,
        None,
        Some(b"two\n".to_vec()),
        Some(b"three\n".to_vec()),
    ];
    assert_eq!(seen, expected, "write, edit (create, replace), apply_patch");
    assert_eq!(f.read("a.txt").unwrap(), b"four\n");
}
