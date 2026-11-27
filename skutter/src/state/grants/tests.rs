use super::*;

/// A grants file in a fresh temp dir — each test its own, injected via `at`
/// (#747), so no process env is shared or mutated.
fn tmp_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("grants.yml");
    (dir, path)
}

async fn always(store: &FileGrantStore, tool: &str, arg: Option<&str>, mode: &str) {
    let s = SessionId::new("s");
    store
        .record(&s, tool, arg, ApprovalScope::Always, mode)
        .await;
}

#[test]
fn grant_key_rule_roundtrips() {
    for key in [
        GrantKey::new("bash", Some("git status"), "build"),
        GrantKey::new("edit", Some("src/main.rs"), "build"),
        GrantKey::new("grep", None, "build"),
    ] {
        assert_eq!(GrantKey::from_rule(&key.to_rule(), key.mode.clone()), key);
    }
    // A malformed key (no closing paren) degrades to a bare tool name.
    assert_eq!(
        GrantKey::from_rule("bash(oops", Some("build".to_string())),
        GrantKey::new("bash(oops", None, "build")
    );
}

#[tokio::test]
async fn a_session_grant_stays_in_memory() {
    let (_dir, path) = tmp_path();
    let store = FileGrantStore::at(Some(path.clone()));
    let s = SessionId::new("s");
    store
        .record(&s, "bash", Some("ls"), ApprovalScope::Session, "build")
        .await;
    assert!(store.is_granted(&s, "bash", Some("ls"), "build"));
    assert!(!path.exists(), "a session grant never touches the file");
    store.forget_session(&s);
    assert!(!store.is_granted(&s, "bash", Some("ls"), "build"));
}

#[tokio::test]
async fn an_always_grant_persists_across_stores_and_modes_stay_apart() {
    let (_dir, path) = tmp_path();
    always(
        &FileGrantStore::at(Some(path.clone())),
        "bash",
        Some("git status"),
        "build",
    )
    .await;

    // A fresh store (a new process) sees it, for any session, in its mode only.
    let other = FileGrantStore::at(Some(path.clone()));
    let any = SessionId::new("other");
    assert!(other.is_granted(&any, "bash", Some("git status"), "build"));
    assert!(!other.is_granted(&any, "bash", Some("git log"), "build"));
    assert!(!other.is_granted(&any, "bash", Some("git status"), "research"));
}

#[tokio::test]
async fn another_instances_grant_counts_without_a_reload() {
    let (_dir, path) = tmp_path();
    let store = FileGrantStore::at(Some(path.clone()));
    let s = SessionId::new("s");
    assert!(!store.is_granted(&s, "grep", None, "build"));
    always(
        &FileGrantStore::at(Some(path.clone())),
        "grep",
        None,
        "build",
    )
    .await;
    assert!(store.is_granted(&s, "grep", None, "build"));
    // Deleting the line revokes it just as directly.
    std::fs::write(&path, "grants: []\n").unwrap();
    assert!(!store.is_granted(&s, "grep", None, "build"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_always_grants_from_two_stores_both_survive() {
    // Two "processes" race to record *different* `Always` grants against the
    // same file (#329): without the lock's read-current-then-merge, the second
    // writer would clobber the first.
    let (_dir, path) = tmp_path();
    let (pa, pb) = (path.clone(), path.clone());
    let a = tokio::spawn(async move {
        always(
            &FileGrantStore::at(Some(pa)),
            "bash",
            Some("git status"),
            "build",
        )
        .await
    });
    let b = tokio::spawn(async move {
        always(
            &FileGrantStore::at(Some(pb)),
            "bash",
            Some("git log"),
            "build",
        )
        .await
    });
    a.await.unwrap();
    b.await.unwrap();

    let reloaded = FileGrantStore::at(Some(path));
    let any = SessionId::new("other");
    assert!(reloaded.is_granted(&any, "bash", Some("git status"), "build"));
    assert!(reloaded.is_granted(&any, "bash", Some("git log"), "build"));
}

#[tokio::test]
async fn a_failed_write_keeps_the_grant_for_this_process() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where the file should be: every write fails.
    let path = dir.path().join("grants.yml");
    std::fs::create_dir(&path).unwrap();
    let store = FileGrantStore::at(Some(path));
    always(&store, "grep", None, "build").await;
    assert!(store.is_granted(&SessionId::new("x"), "grep", None, "build"));
}

#[test]
fn malformed_grants_file_reads_empty() {
    let (_dir, path) = tmp_path();
    std::fs::write(&path, "grants: [oops\n").unwrap();
    let store = FileGrantStore::at(Some(path));
    assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("x"), "build"));
}

/// ADR-0207 §8: a grants file written before modes existed has no `mode` on
/// its entries — loaded as `mode: None`, which never matches a live call.
#[test]
fn legacy_mode_less_grant_never_matches_a_live_call() {
    let (_dir, path) = tmp_path();
    std::fs::write(&path, "grants:\n  - bash(git status)\n").unwrap();
    let store = FileGrantStore::at(Some(path));
    assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("git status"), "build"));
    assert!(!store.is_granted(&SessionId::new("s"), "bash", Some("git status"), "research"));
}

/// A legacy entry survives a later write in its original bare spelling.
#[tokio::test]
async fn a_legacy_entry_round_trips_through_a_new_write() {
    let (_dir, path) = tmp_path();
    std::fs::write(&path, "grants:\n  - bash(git status)\n").unwrap();
    always(
        &FileGrantStore::at(Some(path.clone())),
        "grep",
        None,
        "build",
    )
    .await;
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("- bash(git status)"), "{text}");
    assert!(text.contains("mode: build"), "{text}");
}
