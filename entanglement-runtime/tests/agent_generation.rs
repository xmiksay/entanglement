//! Managed per-agent generation-parameter overrides (#374, ADR-0094): the
//! `agent-generation.yml` store and the `GenerationResolver` closure it builds.
//! Mirrors `agent_models.rs`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use entanglement_core::GenerationParams;
use entanglement_runtime::config::agent_generation::AgentGenerationStore;

/// `ENTANGLEMENT_AGENT_GENERATION_FILE` is process-global; tests that set it serialize.
static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV: &str = "ENTANGLEMENT_AGENT_GENERATION_FILE";

fn tmp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("entanglement-agent-generation-it-{name}.yml"))
}

fn params(temp: f32) -> GenerationParams {
    GenerationParams {
        temperature: Some(temp),
        max_output_tokens: None,
        thinking_budget_tokens: None,
        reasoning_effort: None,
    }
}

#[test]
fn set_round_trips_across_a_reload() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("roundtrip");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(ENV, &path);

    let mut store = AgentGenerationStore::load();
    assert!(store.get("build").is_none());
    store.set("build", params(0.7)).unwrap();

    // A freshly loaded store (a fresh process) sees the persisted override.
    let reloaded = AgentGenerationStore::load();
    assert_eq!(reloaded.get("build"), Some(params(0.7)));

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn env_override_selects_the_file() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("env");
    std::fs::write(&path, "agents:\n  plan:\n    temperature: 0.3\n").unwrap();
    std::env::set_var(ENV, &path);

    let store = AgentGenerationStore::load();
    assert_eq!(store.get("plan"), Some(params(0.3)));

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn malformed_file_loads_empty() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("malformed");
    std::fs::write(&path, "agents: [not-a-map\n").unwrap();
    std::env::set_var(ENV, &path);

    // Fail-open: a corrupt file is ignored, not fatal.
    let store = AgentGenerationStore::load();
    assert!(store.get("build").is_none());

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn resolver_reads_through_to_the_store() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("resolver");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(ENV, &path);

    let mut store = AgentGenerationStore::load();
    store.set("build", params(0.9)).unwrap();
    // A profile the store carries no entry for resolves to `None`, not fatal.
    let shared = Arc::new(Mutex::new(store));
    let resolver = AgentGenerationStore::resolver(shared);

    assert_eq!(resolver("build"), Some(params(0.9)));
    assert_eq!(resolver("ghost"), None);

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn resolver_observes_a_later_set_without_rebuilding() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("resolver-live");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(ENV, &path);

    let store = Arc::new(Mutex::new(AgentGenerationStore::load()));
    let resolver = AgentGenerationStore::resolver(store.clone());
    assert_eq!(resolver("build"), None);

    store.lock().unwrap().set("build", params(0.5)).unwrap();
    // The same closure, called again, observes the write — it reads through the
    // shared handle rather than snapshotting at construction time.
    assert_eq!(resolver("build"), Some(params(0.5)));

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn concurrent_set_from_two_stores_both_survive() {
    // Two "processes" (threads, each with its own `AgentGenerationStore::load()`)
    // race to set *different* agents against the same on-disk file (#329).
    // Without the lock's read-current-then-merge, the second writer's stale
    // in-memory `self.agents` would clobber the first writer's override on write.
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("concurrent");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(ENV, &path);

    let a = std::thread::spawn(|| {
        let mut store = AgentGenerationStore::load();
        store.set("build", params(0.7)).unwrap();
    });
    let b = std::thread::spawn(|| {
        let mut store = AgentGenerationStore::load();
        store.set("plan", params(0.1)).unwrap();
    });
    a.join().unwrap();
    b.join().unwrap();

    let reloaded = AgentGenerationStore::load();
    assert_eq!(
        reloaded.get("build"),
        Some(params(0.7)),
        "override recorded by the first store must survive a concurrent write"
    );
    assert_eq!(
        reloaded.get("plan"),
        Some(params(0.1)),
        "override recorded by the second store must survive a concurrent write"
    );

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reload_picks_up_another_process_write() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("reload");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(ENV, &path);

    let mut store = AgentGenerationStore::load();
    assert!(store.get("build").is_none());

    // Another instance persists an override directly.
    let mut other = AgentGenerationStore::load();
    other.set("build", params(0.7)).unwrap();

    assert!(store.get("build").is_none(), "stale before reload");
    store.reload();
    assert_eq!(store.get("build"), Some(params(0.7)));

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rewrite_is_stable_and_ordered() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = tmp_path("stable");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(ENV, &path);

    let mut store = AgentGenerationStore::load();
    store.set("plan", params(0.1)).unwrap();
    store.set("build", params(0.7)).unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    // Re-setting the identical overrides in the other order rewrites
    // byte-identical output (BTreeMap ⇒ deterministic, `build` before `plan`).
    store.set("build", params(0.7)).unwrap();
    store.set("plan", params(0.1)).unwrap();
    let second = std::fs::read_to_string(&path).unwrap();
    assert_eq!(first, second);
    assert!(first.find("build").unwrap() < first.find("plan").unwrap());

    std::env::remove_var(ENV);
    let _ = std::fs::remove_file(&path);
}
