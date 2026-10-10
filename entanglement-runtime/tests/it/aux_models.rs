//! Managed per-purpose model pins (Issue 5): the `aux-models.yml` store and
//! its resolution through the `AuxLlmRegistry`. Mirrors `agent_models.rs`'s
//! integration test, covering load/save round-trip, set/get, the env-override
//! path, and the registry's fallback behavior.

use std::path::PathBuf;

use entanglement_runtime::config::aux_models::{AuxModelStore, Purpose};

// Every test pins its own temp file through `load_from` — no process env.

fn tmp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("entanglement-aux-models-it-{name}.yml"))
}

#[test]
fn set_round_trips_across_a_reload() {
    let path = tmp_path("roundtrip");
    let _ = std::fs::remove_file(&path);

    let mut store = AuxModelStore::load_from(Some(path.clone()));
    assert!(store.get(Purpose::Summarize).is_none());
    store
        .set(Purpose::Summarize, "zai", "glm-4.5-flash")
        .unwrap();

    // A freshly loaded store (a fresh process) sees the persisted pin.
    let reloaded = AuxModelStore::load_from(Some(path.clone()));
    assert_eq!(
        reloaded.get(Purpose::Summarize),
        Some(("zai", "glm-4.5-flash"))
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_explicit_file_is_read() {
    let path = tmp_path("env");
    std::fs::write(
        &path,
        "aux:\n  session_title:\n    provider: ollama\n    model: llama3.1\n",
    )
    .unwrap();

    let store = AuxModelStore::load_from(Some(path.clone()));
    assert_eq!(
        store.get(Purpose::SessionTitle),
        Some(("ollama", "llama3.1"))
    );
    // The other purpose is unset.
    assert!(store.get(Purpose::Summarize).is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn malformed_file_loads_empty() {
    let path = tmp_path("malformed");
    std::fs::write(&path, "aux: [not-a-map\n").unwrap();

    // Fail-open: a corrupt file is ignored, not fatal.
    let store = AuxModelStore::load_from(Some(path.clone()));
    assert!(store.get(Purpose::Summarize).is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn unrecognized_purpose_key_is_skipped_not_fatal() {
    // A file with a known + an unknown purpose: the known one loads, the
    // unknown one is skipped (debug-logged), and the file is not rejected.
    let path = tmp_path("unknown-key");
    std::fs::write(
        &path,
        "aux:\n  summarize:\n    provider: zai\n    model: glm-4.5-flash\n  bogus_purpose:\n    provider: x\n    model: y\n",
    )
    .unwrap();

    let store = AuxModelStore::load_from(Some(path.clone()));
    assert_eq!(
        store.get(Purpose::Summarize),
        Some(("zai", "glm-4.5-flash"))
    );
    // `bogus_purpose` isn't a known purpose, so it never reaches the
    // enum-keyed map.
    assert!(store.get(Purpose::SessionTitle).is_none());
    assert!(store.get(Purpose::Narrate).is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn deny_unknown_fields_rejects_a_typo() {
    // A typo'd field name makes the file malformed → fail-open to empty.
    let path = tmp_path("typo");
    std::fs::write(
        &path,
        "aux:\n  summarize:\n    provider: zai\n    modle: glm-4.5-flash\n",
    )
    .unwrap();

    let store = AuxModelStore::load_from(Some(path.clone()));
    assert!(store.get(Purpose::Summarize).is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn concurrent_set_from_two_stores_both_survive() {
    // Two "processes" (threads, each with its own `AuxModelStore::load_from(Some(path.clone()))`)
    // race to pin *different* purposes against the same on-disk file (#329).
    let path = tmp_path("concurrent");
    let _ = std::fs::remove_file(&path);

    let a = std::thread::spawn({
        let path = path.clone();
        move || {
            let mut store = AuxModelStore::load_from(Some(path.clone()));
            store
                .set(Purpose::Summarize, "zai", "glm-4.5-flash")
                .unwrap();
        }
    });
    let b = std::thread::spawn({
        let path = path.clone();
        move || {
            let mut store = AuxModelStore::load_from(Some(path.clone()));
            store
                .set(Purpose::SessionTitle, "ollama", "llama3.1")
                .unwrap();
        }
    });
    a.join().unwrap();
    b.join().unwrap();

    let reloaded = AuxModelStore::load_from(Some(path.clone()));
    assert_eq!(
        reloaded.get(Purpose::Summarize),
        Some(("zai", "glm-4.5-flash")),
        "pin recorded by the first store must survive a concurrent write"
    );
    assert_eq!(
        reloaded.get(Purpose::SessionTitle),
        Some(("ollama", "llama3.1")),
        "pin recorded by the second store must survive a concurrent write"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn reload_picks_up_another_process_pin() {
    let path = tmp_path("reload");
    let _ = std::fs::remove_file(&path);

    let mut store = AuxModelStore::load_from(Some(path.clone()));
    assert!(store.get(Purpose::Summarize).is_none());

    // Another instance persists a pin directly.
    let mut other = AuxModelStore::load_from(Some(path.clone()));
    other
        .set(Purpose::Summarize, "zai", "glm-4.5-flash")
        .unwrap();

    assert!(
        store.get(Purpose::Summarize).is_none(),
        "stale before reload"
    );
    store.reload();
    assert_eq!(
        store.get(Purpose::Summarize),
        Some(("zai", "glm-4.5-flash"))
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rewrite_is_stable_and_ordered() {
    let path = tmp_path("stable");
    let _ = std::fs::remove_file(&path);

    let mut store = AuxModelStore::load_from(Some(path.clone()));
    store
        .set(Purpose::SessionTitle, "ollama", "llama3.1")
        .unwrap();
    store
        .set(Purpose::Summarize, "zai", "glm-4.5-flash")
        .unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    // Re-setting the identical pins in the other order rewrites byte-identical
    // output (BTreeMap ⇒ deterministic, `session_title` before `summarize`).
    store
        .set(Purpose::Summarize, "zai", "glm-4.5-flash")
        .unwrap();
    store
        .set(Purpose::SessionTitle, "ollama", "llama3.1")
        .unwrap();
    let second = std::fs::read_to_string(&path).unwrap();
    assert_eq!(first, second);
    assert!(
        first.find("session_title").unwrap() < first.find("summarize").unwrap(),
        "BTreeMap ordering: session_title < summarize"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn persisted_file_carries_the_header_comment() {
    // The managed file starts with the explanatory header so a user discovering
    // it knows it's managed + how to revert.
    let path = tmp_path("header");
    let _ = std::fs::remove_file(&path);

    let mut store = AuxModelStore::load_from(Some(path.clone()));
    store
        .set(Purpose::Summarize, "zai", "glm-4.5-flash")
        .unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.starts_with("# entanglement"),
        "managed file must start with the header comment"
    );
    assert!(
        body.contains("/aux-model"),
        "header must mention the /aux-model command"
    );

    let _ = std::fs::remove_file(&path);
}
