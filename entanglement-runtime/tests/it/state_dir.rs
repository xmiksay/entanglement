//! `ENTANGLEMENT_STATE_DIR` (ADR-0213): one directory relocates all seven
//! managed-state files — and none of the hand-edited inputs.
//!
//! Each store is seeded through its own injection seam at
//! `<dir>/<managed file name>`, then re-loaded through the **production**
//! `load()` with only the env var pointing at `<dir>`: seeing the probe proves
//! the default resolution went there. Probe entries name an agent/tool/server
//! nothing else uses, so a parallel test that happens to resolve a managed
//! file while the var is set reads only inert data.

use std::path::Path;

use entanglement_core::{ApprovalScope, SessionId, StoredAuth, TokenSet, TokenStore};
use entanglement_runtime::config::agent_generation::AgentGenerationStore;
use entanglement_runtime::config::agent_models::AgentModelStore;
use entanglement_runtime::config::aux_models::{AuxModelStore, Purpose};
use entanglement_runtime::config::state_dir::{self, STATE_DIR_ENV};
use entanglement_runtime::config::{Config, McpTokenStore};
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::grants::FileGrantStore;

const PROBE: &str = "state-dir-probe";

fn auth() -> StoredAuth {
    StoredAuth {
        client_id: "cid".into(),
        client_secret: None,
        token_endpoint: "https://as.example/token".into(),
        revocation_endpoint: None,
        resource: None,
        tokens: TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_at: None,
            scope: None,
        },
    }
}

/// Write one probe into each of the seven files under `dir`, by file name.
fn seed(dir: &Path) {
    let file = |name: &str| Some(dir.join(name));
    let session = SessionId::new("seed");
    FileGrantStore::load_from(file(state_dir::GRANTS_FILE)).record(
        &session,
        PROBE,
        None,
        ApprovalScope::Always,
        "build",
    );
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
    McpTokenStore::at(file(state_dir::MCP_TOKENS_FILE))
        .save(PROBE, &auth())
        .unwrap();
    McpTokenStore::at(file(state_dir::LLM_TOKENS_FILE))
        .save(PROBE, &auth())
        .unwrap();
}

#[test]
fn the_state_dir_relocates_all_seven_managed_files() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());

    let _g = crate::env_lock();
    std::env::set_var(STATE_DIR_ENV, dir.path());
    let grants = FileGrantStore::load();
    let agent_models = AgentModelStore::load();
    let agent_generation = AgentGenerationStore::load();
    let aux = AuxModelStore::load();
    let extra_roots = ExtraRootStore::load();
    let mcp = McpTokenStore::load();
    let llm = McpTokenStore::load_llm();
    std::env::remove_var(STATE_DIR_ENV);
    drop(_g);

    assert!(grants.is_granted(&SessionId::new("any"), PROBE, None, "build"));
    assert_eq!(agent_models.get(PROBE), Some(("zai", "glm-5.3")));
    assert!(agent_generation.get(PROBE).is_some());
    assert_eq!(aux.get(Purpose::Narrate), Some(("zai", PROBE)));
    assert!(extra_roots.is_durably_allowed("read", Path::new("/state-dir-probe")));
    assert_eq!(mcp.servers(), vec![PROBE.to_string()]);
    assert_eq!(llm.servers(), vec![PROBE.to_string()]);
}

#[test]
fn the_state_dir_never_moves_a_hand_edited_input() {
    let state = tempfile::tempdir().unwrap();
    // A `config.yml` (and a broken `providers.yml`) parked in the state dir
    // must be invisible: those files keep their own locations/overrides.
    std::fs::write(state.path().join("config.yml"), "agent: state-dir-probe\n").unwrap();
    std::fs::write(state.path().join("providers.yml"), "providers: [oops\n").unwrap();
    let root = tempfile::tempdir().unwrap();

    let _g = crate::env_lock();
    std::env::set_var(STATE_DIR_ENV, state.path());
    std::env::set_var("ENTANGLEMENT_CONFIG_FILE", root.path().join("absent.yml"));
    let config = Config::load(root.path());
    let catalog = entanglement_core::Catalog::load();
    std::env::remove_var("ENTANGLEMENT_CONFIG_FILE");
    std::env::remove_var(STATE_DIR_ENV);
    drop(_g);

    assert_ne!(config.unwrap().agent.as_deref(), Some(PROBE));
    assert!(catalog.is_ok(), "the state dir's providers.yml is not read");
}
