//! Persisted OAuth credentials for MCP servers (ADR-0153).
//!
//! The runtime's [`TokenStore`] implementation: the *policy* half of MCP OAuth,
//! backing the mechanism that lives in `entanglement-provider::mcp::auth`. Owns
//! the managed file `${config_dir}/entanglement/mcp-tokens.yml` (override
//! `ENTANGLEMENT_MCP_TOKENS_FILE`), a sibling of `aux-models.yml` (Issue 5),
//! `agent-models.yml` (#323), the grants file (#174), and the provider-key env
//! file (#220) — **managed, not layered**: the runtime rewrites it freely, so it
//! never mixes into the hand-edited `config.yml`.
//!
//! Shape (a [`BTreeMap`] so the file is stable across rewrites):
//!
//! ```yaml
//! servers:
//!   chessbase:
//!     client_id: s6BhdRkqt3
//!     token_endpoint: https://as.example/token
//!     revocation_endpoint: https://as.example/revoke
//!     tokens:
//!       access_token: "…"
//!       refresh_token: "…"
//!       token_type: Bearer
//!       expires_at: 1780000000
//! ```
//!
//! Unlike every other managed file, this one holds **live credentials**:
//!
//! - Written `0o600` via [`atomic_write`] (which already chmods the temp file
//!   before the rename, so the secret is never briefly world-readable).
//! - Read/modify/write happens under the same advisory file lock (#329) the
//!   other managed files use, so two `skutter` instances can't clobber each
//!   other's refresh.
//! - **Never logged.** The provider's `StoredAuth`/`TokenSet` `Debug` impls
//!   redact every secret, and nothing here prints a token value.
//!
//! A missing file is an empty store. A *malformed* one is logged and treated as
//! empty — fail-open, matching the sibling stores: the worst case is that the
//! user has to re-run `/mcp connect`, whereas failing hard would wedge startup
//! over a file the user cannot easily repair by hand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use entanglement_core::{StoredAuth, TokenStore};
use serde::{Deserialize, Serialize};

use super::atomic::atomic_write;

/// Env var overriding the managed token file path (tests + non-XDG setups).
const MCP_TOKENS_FILE_ENV: &str = "ENTANGLEMENT_MCP_TOKENS_FILE";

/// On-disk shape. A single `servers:` map leaves room for future keys and lets
/// `deny_unknown_fields` flag a typo instead of silently dropping a credential.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpTokensFile {
    #[serde(default)]
    servers: BTreeMap<String, StoredAuth>,
}

/// File-backed [`TokenStore`] for MCP OAuth credentials.
///
/// Holds no in-memory cache: every operation re-reads the file under the lock.
/// Credentials change rarely and the file is tiny, so the simplicity of always
/// reading current state beats caching — and it means a token another instance
/// refreshed is picked up with no invalidation logic.
#[derive(Debug, Clone)]
pub struct McpTokenStore {
    path: Option<PathBuf>,
}

impl McpTokenStore {
    /// Resolve the managed file path from `ENTANGLEMENT_MCP_TOKENS_FILE` or
    /// `${config_dir}/entanglement/mcp-tokens.yml`. A store with no resolvable
    /// path reads empty and refuses writes with a clear error.
    pub fn load() -> Self {
        Self {
            path: tokens_file_path(),
        }
    }

    /// Every server with a stored credential — the `/mcp list` "authenticated"
    /// column, and what lets startup decide which servers to connect with auth.
    pub fn servers(&self) -> Vec<String> {
        match &self.path {
            Some(p) => read_tokens(p).into_keys().collect(),
            None => Vec::new(),
        }
    }

    /// Is there a stored credential for `server`? Cheaper to express than
    /// `load(..).is_some()` at the call sites that only need the boolean.
    pub fn has(&self, server: &str) -> bool {
        self.load_entry(server).is_some()
    }

    fn load_entry(&self, server: &str) -> Option<StoredAuth> {
        let path = self.path.as_ref()?;
        read_tokens(path).remove(server)
    }

    fn require_path(&self) -> Result<PathBuf> {
        match self.path.clone() {
            Some(p) => Ok(p),
            None => bail!(
                "no config directory for the managed MCP token file; \
                 set {MCP_TOKENS_FILE_ENV} to a path first"
            ),
        }
    }
}

impl TokenStore for McpTokenStore {
    fn load(&self, server: &str) -> Result<Option<StoredAuth>> {
        Ok(self.load_entry(server))
    }

    /// Merge this server's credential into whatever is on disk, under the
    /// exclusive lock — a concurrent instance's own `connect`/refresh for a
    /// *different* server must survive rather than being clobbered by a write
    /// from stale in-memory state (#329).
    fn save(&self, server: &str, auth: &StoredAuth) -> Result<()> {
        let path = self.require_path()?;
        super::lock::with_locked_file(&path, || {
            let mut on_disk = read_tokens(&path);
            on_disk.insert(server.to_string(), auth.clone());
            persist_map(&path, &on_disk)
        })
    }

    /// Drop this server's credential. Deleting an absent entry is a no-op, not
    /// an error — `/mcp disconnect` on an unauthenticated server should report
    /// "not authorized", not fail.
    fn delete(&self, server: &str) -> Result<()> {
        let path = self.require_path()?;
        super::lock::with_locked_file(&path, || {
            let mut on_disk = read_tokens(&path);
            if on_disk.remove(server).is_none() {
                return Ok(());
            }
            persist_map(&path, &on_disk)
        })
    }
}

/// Re-write the managed file from `servers`. The [`BTreeMap`] keeps the output
/// stable across rewrites.
fn persist_map(path: &Path, servers: &BTreeMap<String, StoredAuth>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let doc = McpTokensFile {
        servers: servers.clone(),
    };
    let body = serde_yaml::to_string(&doc)?;
    let header = "# entanglement — OAuth credentials for MCP servers (ADR-0153).\n\
                  # Managed by skutter: written by /mcp connect, refreshed automatically,\n\
                  # removed by /mcp disconnect. THIS FILE CONTAINS LIVE ACCESS TOKENS —\n\
                  # it is written 0600; do not commit it or share it.\n";
    atomic_write(path, &format!("{header}{body}"))
}

/// Read + parse the token file. A missing file, or any read/parse error, yields
/// an empty map (logged without any file content, which could carry a token).
fn read_tokens(path: &Path) -> BTreeMap<String, StoredAuth> {
    if !path.exists() {
        return BTreeMap::new();
    }
    match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_yaml::from_str::<McpTokensFile>(&t).map_err(|e| e.to_string()))
    {
        Ok(file) => file.servers,
        Err(e) => {
            // The error string is serde's own (a line/column and field name),
            // never a value — safe to log, unlike the file body.
            tracing::warn!(
                path = %path.display(),
                "mcp-tokens: could not parse the managed token file ({e}); treating as empty — \
                 re-run `/mcp connect <server>` to re-authorize"
            );
            BTreeMap::new()
        }
    }
}

fn tokens_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(MCP_TOKENS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("mcp-tokens.yml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::TokenSet;

    /// `ENTANGLEMENT_MCP_TOKENS_FILE` is process-global; tests that set it
    /// serialize here so they don't race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn store_at(label: &str) -> (McpTokenStore, PathBuf) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = std::env::temp_dir().join(format!(
            "entanglement-mcp-tokens-{label}-{}.yml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::env::set_var(MCP_TOKENS_FILE_ENV, &path);
        let store = McpTokenStore::load();
        std::env::remove_var(MCP_TOKENS_FILE_ENV);
        (store, path)
    }

    fn auth(access: &str) -> StoredAuth {
        StoredAuth {
            client_id: "cid".into(),
            client_secret: None,
            token_endpoint: "https://as.example/token".into(),
            revocation_endpoint: Some("https://as.example/revoke".into()),
            resource: None,
            tokens: TokenSet {
                access_token: access.into(),
                refresh_token: Some("rt".into()),
                token_type: "Bearer".into(),
                expires_at: Some(9_999_999_999),
                scope: None,
            },
        }
    }

    #[test]
    fn save_load_delete_round_trip() {
        let (store, path) = store_at("roundtrip");
        assert!(store.load("srv").unwrap().is_none());
        assert!(!store.has("srv"));

        store.save("srv", &auth("token-a")).unwrap();
        let got = store.load("srv").unwrap().unwrap();
        assert_eq!(got.tokens.access_token, "token-a");
        assert_eq!(got.client_id, "cid");
        assert!(store.has("srv"));
        assert_eq!(store.servers(), vec!["srv".to_string()]);

        store.delete("srv").unwrap();
        assert!(store.load("srv").unwrap().is_none());
        // Deleting again is a no-op, not an error.
        store.delete("srv").unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saving_one_server_preserves_the_others() {
        let (store, path) = store_at("multi");
        store.save("a", &auth("ta")).unwrap();
        store.save("b", &auth("tb")).unwrap();
        assert_eq!(store.servers(), vec!["a".to_string(), "b".to_string()]);
        // A delete of one leaves the other intact.
        store.delete("a").unwrap();
        assert!(store.load("b").unwrap().is_some());
        assert_eq!(store.load("b").unwrap().unwrap().tokens.access_token, "tb");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_file_reads_as_empty_rather_than_failing() {
        let (store, path) = store_at("malformed");
        std::fs::write(&path, "servers: [this is not a map]\n").unwrap();
        assert!(store.load("srv").unwrap().is_none());
        assert!(store.servers().is_empty());
        // And the store is still writable afterwards (fail-open, not wedged).
        store.save("srv", &auth("recovered")).unwrap();
        assert_eq!(
            store.load("srv").unwrap().unwrap().tokens.access_token,
            "recovered"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_key_in_the_file_is_rejected_not_silently_dropped() {
        let (store, path) = store_at("unknown-key");
        std::fs::write(&path, "serverz:\n  a: {}\n").unwrap();
        // `deny_unknown_fields` → parse error → fail-open empty.
        assert!(store.servers().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persisted_file_carries_the_warning_header_and_is_owner_only() {
        let (store, path) = store_at("perms");
        store.save("srv", &auth("secret-value")).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("# entanglement — OAuth credentials"));
        assert!(body.contains("LIVE ACCESS TOKENS"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_store_with_no_path_reads_empty_and_refuses_writes() {
        let store = McpTokenStore { path: None };
        assert!(store.load("srv").unwrap().is_none());
        assert!(store.servers().is_empty());
        let err = store.save("srv", &auth("x")).unwrap_err();
        assert!(err.to_string().contains(MCP_TOKENS_FILE_ENV));
    }
}
