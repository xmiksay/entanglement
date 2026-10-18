//! The host tools stamp a change *before* they write (ADR-0236 §2): the
//! ancestry probe must still see the file's pre-write state.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use entanglement_core::SessionId;

use super::tests::Fixture;
use crate::host::{ApplyPatchTool, EditTool, WriteTool};
use crate::tools::Tool;

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

async fn run(tool: &dyn Tool, f: &Fixture, input: serde_json::Value) {
    tool.run_for_session(&f.sid, "req", &input.to_string())
        .await
        .unwrap();
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
