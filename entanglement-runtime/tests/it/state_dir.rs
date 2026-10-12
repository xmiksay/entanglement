//! `ENTANGLEMENT_STATE_DIR` (ADR-0213): one directory relocates all eight
//! managed-state files (the eighth, `mcp-tools.yml`, ADR-0226). (That it moves none of the hand-edited inputs is
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

use entanglement_core::{ApprovalScope, SessionId, StoredAuth, TokenSet, TokenStore};
use entanglement_runtime::config::agent_generation::AgentGenerationStore;
use entanglement_runtime::config::agent_models::AgentModelStore;
use entanglement_runtime::config::aux_models::{AuxModelStore, Purpose};
use entanglement_runtime::config::state_dir::{self, STATE_DIR_ENV};
use entanglement_runtime::config::McpTokenStore;
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::grants::FileGrantStore;
use entanglement_runtime::mcp::McpToolCache;

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

/// Write one probe into each of the eight files under `dir`, by file name.
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
    McpToolCache::at(file(state_dir::MCP_TOOLS_FILE))
        .record(PROBE, "probe-target", &[PROBE.to_string()])
        .unwrap();
}

#[test]
fn the_state_dir_relocates_all_eight_managed_files() {
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
    let tool_cache = McpToolCache::managed();
    let seen = tool_cache.lookup(PROBE, "probe-target");
    std::env::remove_var(STATE_DIR_ENV);
    drop(_g);

    assert!(grants.is_granted(&SessionId::new("any"), PROBE, None, "build"));
    assert_eq!(agent_models.get(PROBE), Some(("zai", "glm-5.3")));
    assert!(agent_generation.get(PROBE).is_some());
    assert_eq!(aux.get(Purpose::Narrate), Some(("zai", PROBE)));
    assert!(extra_roots.is_durably_allowed("read", Path::new("/state-dir-probe")));
    assert_eq!(mcp.servers(), vec![PROBE.to_string()]);
    assert_eq!(llm.servers(), vec![PROBE.to_string()]);
    assert_eq!(seen, Some(vec![PROBE.to_string()]));
}
