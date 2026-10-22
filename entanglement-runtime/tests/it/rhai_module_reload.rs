//! ADR-0248: the definitions watcher re-runs rhai module discovery — a new
//! module becomes importable, and closing an import cycle makes its modules
//! unimportable, in the live skill registry every script run reads.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use entanglement_runtime::config::agent_models::AgentModelStore;
use entanglement_runtime::policy::DefaultGrantStore;
use entanglement_runtime::session_store::SessionStore;
use entanglement_runtime::skills::{load_registry, SkillRegistry};
use entanglement_runtime::slash_commands::CommandRegistry;
use entanglement_runtime::watch::{spawn_watcher, LiveDefinitions};

fn importable(skills: &RwLock<Arc<SkillRegistry>>, name: &str) -> bool {
    let reg = skills.read().unwrap().clone();
    reg.modules.get(name).is_some_and(|m| m.lib.is_ok())
}

fn write(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).unwrap();
}

#[tokio::test]
async fn the_watcher_rediscovers_modules_and_cycles() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("proj");
    let dir = root.join(".entanglement/rhai/modules");
    std::fs::create_dir_all(&dir).unwrap();
    let skills = Arc::new(RwLock::new(Arc::new(load_registry(&root).unwrap())));
    assert!(importable(&skills, "json_schema") && !importable(&skills, "a"));
    let live = LiveDefinitions::new(
        Arc::new(RwLock::new(
            entanglement_runtime::agents::built_in_registry().unwrap(),
        )),
        skills.clone(),
        Arc::new(RwLock::new(Arc::new(CommandRegistry::default()))),
        Arc::new(Mutex::new(AgentModelStore::load_from(None))),
        Arc::new(DefaultGrantStore::load_from(None)),
        HashMap::new(),
    )
    // A reload's prompt context creates its scratch dir: keep it here.
    .with_sessions(SessionStore::at(tmp.path().join("sessions")));
    let _watcher = spawn_watcher(root.clone(), live, None).unwrap();
    // Let the watch register before the first write.
    tokio::time::sleep(Duration::from_millis(200)).await;

    write(&dir, "b.rhai", "fn f() { 1 }");
    write(&dir, "a.rhai", "import \"b\" as b;\nfn f() { b::f() }");
    let loaded = || importable(&skills, "a") && importable(&skills, "b");
    assert!(super::eventually(loaded).await, "the new modules load");

    write(&dir, "b.rhai", "import \"a\" as a;\nfn f() { 1 }");
    let cycled = || !importable(&skills, "a") && !importable(&skills, "b");
    assert!(super::eventually(cycled).await, "the cycle refuses both");

    write(&dir, "b.rhai", "fn f() { 2 }");
    assert!(
        super::eventually(loaded).await,
        "breaking the cycle heals both"
    );
}
