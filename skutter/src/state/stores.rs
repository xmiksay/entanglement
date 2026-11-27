//! The file-backed stores `skutter`'s live engine runs on (#966), built and
//! wired in one place. The library defaults every seam to memory, so a head
//! that forgot one would lose credentials, last-seen tool names or `Always`
//! grants at exit with no error — [`Stores`] is the single value boot hands
//! to the MCP roster ([`Stores::mcp_roster`]) and the executor (`grants`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::config::state_dir::{GRANTS_FILE, MCP_TOKENS_FILE, MCP_TOOLS_FILE};
use entanglement_runtime::mcp::{AvailableMcp, McpServerConfig};

use super::{FileGrantStore, FileMcpToolCache, McpTokenStore};

/// `skutter`'s managed-state files behind the library's in-memory seams.
pub struct Stores {
    pub grants: Arc<FileGrantStore>,
    pub tokens: Arc<McpTokenStore>,
    pub tool_cache: Arc<FileMcpToolCache>,
}

impl Stores {
    /// The managed files in the state dir (`ENTANGLEMENT_STATE_DIR`).
    pub fn load() -> Self {
        Self {
            grants: Arc::new(FileGrantStore::load()),
            tokens: Arc::new(McpTokenStore::load()),
            tool_cache: Arc::new(FileMcpToolCache::managed()),
        }
    }

    /// The managed files under an explicit `dir` — the injection seam (#747).
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            grants: Arc::new(FileGrantStore::at(Some(dir.join(GRANTS_FILE)))),
            tokens: Arc::new(McpTokenStore::at(Some(dir.join(MCP_TOKENS_FILE)))),
            tool_cache: Arc::new(FileMcpToolCache::at(Some(dir.join(MCP_TOOLS_FILE)))),
        }
    }

    /// The MCP roster of `servers` over these token and tool-name stores.
    pub fn mcp_roster(
        &self,
        servers: &HashMap<String, McpServerConfig>,
        secret_env: Vec<String>,
    ) -> AvailableMcp {
        AvailableMcp::from_config(servers, secret_env)
            .with_tool_cache(self.tool_cache.clone())
            .with_token_store(self.tokens.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{ApprovalScope, SessionId, StoredAuth, TokenSet, TokenStore};
    use entanglement_runtime::mcp::McpToolCache;
    use entanglement_runtime::policy::GrantStore;

    fn oauth_server() -> McpServerConfig {
        serde_yaml::from_str("url: https://kb.example/mcp\noauth: {}\n").unwrap()
    }

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

    /// What one process stored, the next one's roster and grant store see:
    /// every seam is backed by its file under the state dir.
    #[tokio::test]
    async fn the_roster_and_grants_are_file_backed() {
        let dir = tempfile::tempdir().unwrap();
        let before = Stores::in_dir(dir.path());
        before.tokens.save("kb", &auth()).unwrap();
        before
            .tool_cache
            .record("kb", "https://kb.example/mcp", &["search".to_string()])
            .unwrap();
        before
            .grants
            .record(
                &SessionId::new("s"),
                "grep",
                None,
                ApprovalScope::Always,
                "build",
            )
            .await;

        let after = Stores::in_dir(dir.path());
        let servers = HashMap::from([("kb".to_string(), oauth_server())]);
        let roster = after.mcp_roster(&servers, Vec::new());
        assert!(!roster.needs_auth("kb", &oauth_server()));
        assert!(roster.token_store().load("kb").unwrap().is_some());
        assert_eq!(
            roster.tool_cache().lookup("kb", "https://kb.example/mcp"),
            Some(vec!["search".to_string()])
        );
        assert!(after
            .grants
            .is_granted(&SessionId::new("other"), "grep", None, "build"));
    }
}
