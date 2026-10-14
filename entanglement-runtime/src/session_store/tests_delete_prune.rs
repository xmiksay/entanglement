//! Tests for `SessionStore::delete` and `SessionStore::prune`
//! (Issue 4, Phases 4.1 + 4.2).

use super::*;
use entanglement_core::OutEvent;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Write a minimal `.jsonl` session file into `store` for `id`.
fn write_session_file(store: &SessionStore, id: &SessionId) -> PathBuf {
    let record = LogRecord::new(
        id.clone(),
        LogPayload::Out(OutEvent::SessionStarted {
            session: id.clone(),
            parent: None,
            predecessor: None,
            agent: "build".to_string(),
            model: None,
            root: true,
            ts: 1000,
            user: None,
        }),
    );
    store.append(id, &record).expect("append should succeed");
    store.session_path(id).expect("session_path should succeed")
}

/// Backdate a file's mtime by `days` days, simulating a stale session log.
/// Uses `std::fs::File::set_modified` (stable since 1.75).
fn backdate(path: &Path, days: u64) {
    let old = SystemTime::now() - Duration::from_secs(days * 86_400);
    File::open(path)
        .and_then(|f| f.set_modified(old))
        .unwrap_or_else(|e| panic!("set_modified({}) failed: {e}", path.display()));
}

#[test]
fn delete_removes_a_session_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let id = SessionId::new("doomed");

    let path = write_session_file(&store, &id);
    assert!(path.exists(), "file should exist before delete");

    store.delete(&id).expect("delete should succeed");

    assert!(!path.exists(), "file should be gone after delete");
}

#[test]
fn delete_errors_on_a_missing_file() {
    // A missing file is a real error (not silent success) so a stale UI row
    // or a double-`d` press is surfaced rather than swallowed.
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let id = SessionId::new("never-existed");

    let err = store.delete(&id).unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("delete"),
        "error should mention delete: {err:#}"
    );
}

#[test]
fn delete_only_removes_the_named_session() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let keep = SessionId::new("keep");
    let drop_id = SessionId::new("drop");

    let keep_path = write_session_file(&store, &keep);
    let drop_path = write_session_file(&store, &drop_id);

    store.delete(&drop_id).expect("delete should succeed");

    assert!(!drop_path.exists(), "dropped file gone");
    assert!(keep_path.exists(), "sibling file untouched");

    // Listing reflects the survivor only.
    let sessions = store.list_sessions().expect("list should succeed");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, keep);
}

#[test]
fn prune_removes_only_stale_files() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());

    let fresh = SessionId::new("fresh");
    let stale = SessionId::new("stale");
    let fresh_path = write_session_file(&store, &fresh);
    let stale_path = write_session_file(&store, &stale);

    // Backdate the stale one by 60 days; prune at a 30-day threshold.
    backdate(&stale_path, 60);

    let removed = store.prune(30).expect("prune should succeed");
    assert_eq!(removed, 1, "exactly the stale file");
    assert!(!stale_path.exists(), "stale file pruned");
    assert!(fresh_path.exists(), "fresh file kept");
}

#[test]
fn prune_with_zero_threshold_removes_everything() {
    // A 0-day retention is "delete everything older than now" — boundary check
    // that the cutoff math doesn't accidentally keep files at/just-before now.
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());

    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let a_path = write_session_file(&store, &a);
    let b_path = write_session_file(&store, &b);
    // Backdate both by 1 day so they're unambiguously older than a 0-day cutoff.
    backdate(&a_path, 1);
    backdate(&b_path, 1);

    let removed = store.prune(0).expect("prune should succeed");
    assert_eq!(removed, 2);
    assert!(!a_path.exists());
    assert!(!b_path.exists());
}

#[test]
fn prune_with_large_threshold_keeps_everything() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());

    let id = SessionId::new("keeper");
    let path = write_session_file(&store, &id);
    // Even backdated by a year, a 3650-day (10-year) threshold keeps it.
    backdate(&path, 365);

    let removed = store.prune(3650).expect("prune should succeed");
    assert_eq!(removed, 0);
    assert!(path.exists());
}

#[test]
fn prune_ignores_non_jsonl_files() {
    // A stray non-`.jsonl` file (e.g. a temp file, a `.lock`) must be left
    // alone — prune only touches session logs.
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let dir = store.dir().expect("session dir");

    let junk = dir.join("notes.txt");
    std::fs::write(&junk, "hello").expect("write");
    backdate(&junk, 365);

    let removed = store.prune(1).expect("prune should succeed");
    assert_eq!(removed, 0);
    assert!(junk.exists(), "non-jsonl file untouched");
}

#[test]
fn prune_does_not_walk_sibling_project_dirs() {
    // Prune scope is the store's own dir only (Issue 4 spec). A sibling
    // project's session dir under the same base must not be touched.
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path().join("this-project"));
    store.dir().expect("session dir");
    let other_project = temp.path().join("some-other-project");
    std::fs::create_dir_all(&other_project).expect("mkdir");
    let other_log = other_project.join("stranger.jsonl");
    std::fs::write(&other_log, "{}\n").expect("write");
    backdate(&other_log, 365);

    let removed = store.prune(1).expect("prune should succeed");
    assert_eq!(removed, 0);
    assert!(other_log.exists(), "sibling project dir untouched");
}

#[test]
#[should_panic(expected = "resolved the real user data dir")]
fn for_cwd_refuses_the_real_data_dir_under_test() {
    // The #747 guard: a unit test can never reach the developer's live logs.
    let _ = SessionStore::for_cwd(Path::new("/some/project"));
}

/// ADR-0234: prune stays stat-only. A file moved sub-agents were adopted
/// from is kept because the tap `touch`ed it when it wrote the pointer, not
/// because prune read anything — every file here is unparseable garbage, and
/// an unrelated stale one is still pruned.
#[test]
fn prune_keeps_a_just_pointed_file_by_its_fresh_mtime_alone() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let (pointed, unrelated) = (SessionId::new("pointed"), SessionId::new("unrelated"));
    for s in [&pointed, &unrelated] {
        let path = store.session_path(s).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not a log record\n").unwrap();
        backdate(&path, 40);
    }
    store.touch(&pointed).unwrap();
    store.touch(&SessionId::new("never-written")).unwrap();

    assert_eq!(store.prune(30).unwrap(), 1);
    assert!(store.session_path(&pointed).unwrap().exists());
    assert!(!store.session_path(&unrelated).unwrap().exists());
    assert!(!store
        .session_path(&SessionId::new("never-written"))
        .unwrap()
        .exists());
}
