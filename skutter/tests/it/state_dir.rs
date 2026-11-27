//! `ENTANGLEMENT_STATE_DIR` (ADR-0213) relocates `skutter`'s own managed
//! files too (#966): the grants, the MCP OAuth credentials and the last-seen
//! MCP tool names. Each store is seeded at `<dir>/<managed file name>` through
//! its injection seam, then re-loaded through the **production** constructor
//! with only the env var pointing at `<dir>`: seeing the probe proves the
//! default resolution went there.

use entanglement_core::{ApprovalScope, SessionId, StoredAuth, TokenSet, TokenStore};
use entanglement_runtime::config::state_dir::{
    GRANTS_FILE, MCP_TOKENS_FILE, MCP_TOOLS_FILE, STATE_DIR_ENV,
};
use entanglement_runtime::mcp::McpToolCache;
use entanglement_runtime::policy::GrantStore;
use skutter::state::{FileGrantStore, FileMcpToolCache, McpTokenStore};

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

#[tokio::test]
async fn the_state_dir_relocates_skutters_managed_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str| Some(dir.path().join(name));
    FileGrantStore::at(file(GRANTS_FILE))
        .record(
            &SessionId::new("seed"),
            PROBE,
            None,
            ApprovalScope::Always,
            "build",
        )
        .await;
    McpTokenStore::at(file(MCP_TOKENS_FILE))
        .save(PROBE, &auth())
        .unwrap();
    FileMcpToolCache::at(file(MCP_TOOLS_FILE))
        .record(PROBE, "probe-target", &[PROBE.to_string()])
        .unwrap();

    // No other test in this binary reads the state dir in-process; spawned
    // `skutter` children get their own env (`skutter_cmd`).
    std::env::set_var(STATE_DIR_ENV, dir.path());
    let grants = FileGrantStore::load();
    let tokens = McpTokenStore::load();
    let cache = FileMcpToolCache::managed();
    std::env::remove_var(STATE_DIR_ENV);

    assert!(grants.is_granted(&SessionId::new("any"), PROBE, None, "build"));
    assert_eq!(tokens.servers(), vec![PROBE.to_string()]);
    assert_eq!(
        cache.lookup(PROBE, "probe-target"),
        Some(vec![PROBE.to_string()])
    );
}
