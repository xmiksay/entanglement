//! Managed per-agent model pins (#323, ADR-0081): the `agent-models.yml` store
//! and its overlay onto a loaded profile registry.

use std::path::PathBuf;

use entanglement_core::{Agent, AgentCatalog};
use entanglement_runtime::config::agent_models::AgentModelStore;

// Every test pins its own temp file through `load_from` — no process env.

/// A fresh directory per test, removed with the returned guard (lock file
/// included): concurrent test processes must not share one file.
fn tmp_path(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir
        .path()
        .join(format!("entanglement-agent-models-it-{name}.yml"));
    (dir, path)
}

fn profile(name: &str) -> Agent {
    Agent {
        name: name.to_string(),
        description: "d".into(),
        system_prompt: String::new(),
        model: None,
        provider: None,
        default_mode: None,
    }
}

#[test]
fn set_round_trips_across_a_reload() {
    let (_dir, path) = tmp_path("roundtrip");
    let _ = std::fs::remove_file(&path);

    let mut store = AgentModelStore::load_from(Some(path.clone()));
    assert!(store.get("build").is_none());
    store.set("build", "zai", "glm-5.2").unwrap();

    // A freshly loaded store (a fresh process) sees the persisted pin.
    let reloaded = AgentModelStore::load_from(Some(path.clone()));
    assert_eq!(reloaded.get("build"), Some(("zai", "glm-5.2")));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_explicit_file_is_read() {
    let (_dir, path) = tmp_path("env");
    std::fs::write(
        &path,
        "agents:\n  plan:\n    provider: anthropic\n    model: claude\n",
    )
    .unwrap();

    let store = AgentModelStore::load_from(Some(path.clone()));
    assert_eq!(store.get("plan"), Some(("anthropic", "claude")));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn malformed_file_loads_empty() {
    let (_dir, path) = tmp_path("malformed");
    std::fs::write(&path, "agents: [not-a-map\n").unwrap();

    // Fail-open: a corrupt file is ignored, not fatal.
    let store = AgentModelStore::load_from(Some(path.clone()));
    assert!(store.get("build").is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn apply_overlays_pins_and_wins_over_frontmatter() {
    let (_dir, path) = tmp_path("apply");
    let _ = std::fs::remove_file(&path);

    let mut store = AgentModelStore::load_from(Some(path.clone()));
    store.set("build", "anthropic", "claude").unwrap();
    // A pin for a profile the registry doesn't carry is ignored, not fatal.
    store.set("ghost", "x", "y").unwrap();

    let mut reg = AgentCatalog::default();
    // `build` carries a frontmatter pin the persisted store must override.
    let mut build = profile("build");
    build.provider = Some("zai".into());
    build.model = Some("glm-5.2".into());
    reg.insert(build);
    // `plan` has no persisted pin, so its (absent) binding is untouched.
    reg.insert(profile("plan"));

    store.apply(&mut reg);

    assert_eq!(
        reg.get("build").unwrap().model_pin(),
        Some(("anthropic", "claude")),
        "persisted pin wins over frontmatter"
    );
    assert_eq!(reg.get("plan").unwrap().model_pin(), None);
    assert!(reg.get("ghost").is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn concurrent_set_from_two_stores_both_survive() {
    // Two "processes" (threads, each with its own `AgentModelStore::load_from(Some(path.clone()))`)
    // race to pin *different* agents against the same on-disk file (#329).
    // Without the lock's read-current-then-merge, the second writer's stale
    // in-memory `self.agents` would clobber the first writer's pin on write.
    let (_dir, path) = tmp_path("concurrent");
    let _ = std::fs::remove_file(&path);

    let a = std::thread::spawn({
        let path = path.clone();
        move || {
            let mut store = AgentModelStore::load_from(Some(path.clone()));
            store.set("build", "zai", "glm-5.2").unwrap();
        }
    });
    let b = std::thread::spawn({
        let path = path.clone();
        move || {
            let mut store = AgentModelStore::load_from(Some(path.clone()));
            store.set("plan", "anthropic", "claude").unwrap();
        }
    });
    a.join().unwrap();
    b.join().unwrap();

    let reloaded = AgentModelStore::load_from(Some(path.clone()));
    assert_eq!(
        reloaded.get("build"),
        Some(("zai", "glm-5.2")),
        "pin recorded by the first store must survive a concurrent write"
    );
    assert_eq!(
        reloaded.get("plan"),
        Some(("anthropic", "claude")),
        "pin recorded by the second store must survive a concurrent write"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn reload_picks_up_another_process_pin() {
    let (_dir, path) = tmp_path("reload");
    let _ = std::fs::remove_file(&path);

    let mut store = AgentModelStore::load_from(Some(path.clone()));
    assert!(store.get("build").is_none());

    // Another instance persists a pin directly.
    let mut other = AgentModelStore::load_from(Some(path.clone()));
    other.set("build", "zai", "glm-5.2").unwrap();

    assert!(store.get("build").is_none(), "stale before reload");
    store.reload();
    assert_eq!(store.get("build"), Some(("zai", "glm-5.2")));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rewrite_is_stable_and_ordered() {
    let (_dir, path) = tmp_path("stable");
    let _ = std::fs::remove_file(&path);

    let mut store = AgentModelStore::load_from(Some(path.clone()));
    store.set("plan", "zai", "glm-plan").unwrap();
    store.set("build", "zai", "glm-5.2").unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    // Re-setting the identical pins in the other order rewrites byte-identical
    // output (BTreeMap ⇒ deterministic, `build` before `plan`).
    store.set("build", "zai", "glm-5.2").unwrap();
    store.set("plan", "zai", "glm-plan").unwrap();
    let second = std::fs::read_to_string(&path).unwrap();
    assert_eq!(first, second);
    assert!(first.find("build").unwrap() < first.find("plan").unwrap());

    let _ = std::fs::remove_file(&path);
}
