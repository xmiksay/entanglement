//! `ENTANGLEMENT_STATE_DIR` (ADR-0213): one directory relocates every
//! managed-state file. The four stores the library keeps are checked here;
//! `skutter`'s own three (grants, MCP tokens, MCP tool names, #966) in its
//! `state_dir` test. (That it moves none of the hand-edited inputs is
//! unit-tested on each input's pure path resolver — `config.yml`, `.env`,
//! `providers.yml` — without touching process env or the real config dir.)
//!
//! Each store is seeded through its own injection seam at
//! `<dir>/<managed file name>`, then re-loaded through the **production**
//! `load()` with only the env var pointing at `<dir>`: seeing the probe proves
//! the default resolution went there. Probe entries name an agent/tool/server
//! nothing else uses, so a parallel test that happens to resolve a managed
//! file while the var is set reads only inert data.

use std::path::Path;

use entanglement_core::ApprovalScope;
use entanglement_runtime::config::agent_generation::AgentGenerationStore;
use entanglement_runtime::config::agent_models::AgentModelStore;
use entanglement_runtime::config::aux_models::{AuxModelStore, Purpose};
use entanglement_runtime::config::state_dir::{self, STATE_DIR_ENV};
use entanglement_runtime::extra_roots::ExtraRootStore;

const PROBE: &str = "state-dir-probe";

/// Write one probe into each of the library's four files under `dir`, by
/// file name.
fn seed(dir: &Path) {
    let file = |name: &str| Some(dir.join(name));
    AgentModelStore::load_from(file(state_dir::AGENT_MODELS_FILE))
        .set(PROBE, "zai", "glm-5.3")
        .unwrap();
    AgentGenerationStore::load_from(file(state_dir::AGENT_GENERATION_FILE))
        .set(PROBE, Default::default())
        .unwrap();
    AuxModelStore::load_from(file(state_dir::AUX_MODELS_FILE))
        .set(Purpose::Narrate, "zai", PROBE)
        .unwrap();
    ExtraRootStore::load_from(file(state_dir::EXTRA_ROOTS_FILE)).record(
        "read",
        Path::new("/state-dir-probe"),
        ApprovalScope::Always,
        "req",
    );
}

#[test]
fn the_state_dir_relocates_the_library_managed_files() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());

    let _g = crate::env_lock();
    std::env::set_var(STATE_DIR_ENV, dir.path());
    let agent_models = AgentModelStore::load();
    let agent_generation = AgentGenerationStore::load();
    let aux = AuxModelStore::load();
    let extra_roots = ExtraRootStore::load();
    std::env::remove_var(STATE_DIR_ENV);
    drop(_g);

    assert_eq!(agent_models.get(PROBE), Some(("zai", "glm-5.3")));
    assert!(agent_generation.get(PROBE).is_some());
    assert_eq!(aux.get(Purpose::Narrate), Some(("zai", PROBE)));
    assert!(extra_roots.is_durably_allowed("read", Path::new("/state-dir-probe")));
}
