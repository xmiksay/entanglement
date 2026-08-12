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
//!   other's write. [`TokenStore::with_exclusive`] extends that same lock to
//!   cover the refresh *exchange* too, not just the write that follows it —
//!   closing the cross-process refresh race ADR-0153 originally accepted for
//!   v1 (#631).
//! - **Never logged.** The provider's `StoredAuth`/`TokenSet` `Debug` impls
//!   redact every secret, and nothing here prints a token value.
//!
//! A missing file is an empty store. A *malformed* one is logged and treated as
//! empty for reads — fail-open, matching the sibling stores: the worst case is
//! that the user has to re-run `/mcp connect`, whereas failing hard would wedge
//! startup over a file the user cannot easily repair by hand.
//!
//! A write (`save`/`delete`) is the destructive case: read-modify-write over a
//! "treated as empty" map would silently discard every *other* server's
//! credential the moment any one server reconnects (#549). So a write that
//! finds an unparseable file moves it aside to a `.corrupt-<unix-secs>`
//! sibling first and starts fresh from empty, rather than overwriting the
//! original bytes — the operator can still recover a refresh token from the
//! moved-aside file by hand. If the move itself fails, the write bails instead
//! of touching the file at all.
//!
//! No `deny_unknown_fields` on [`McpTokensFile`]: a version downgrade or a
//! forward-compat schema addition must never turn into a parse error on this
//! file, since (per the above) a parse error on a write is destructive. Typos
//! in the credentials the store actually manages are still structurally
//! checked, via [`StoredAuth`]'s own required fields.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use entanglement_core::{StoredAuth, TokenStore};
use serde::{Deserialize, Serialize};

use super::atomic::atomic_write;

/// Env var overriding the managed token file path (tests + non-XDG setups).
const MCP_TOKENS_FILE_ENV: &str = "ENTANGLEMENT_MCP_TOKENS_FILE";
/// Env var overriding the managed *LLM* token file path (#684 edge d).
const LLM_TOKENS_FILE_ENV: &str = "ENTANGLEMENT_LLM_TOKENS_FILE";

/// On-disk shape: a single `servers:` map.
#[derive(Debug, Default, Serialize, Deserialize)]
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

    /// The sibling store for OAuth-protected **LLM provider endpoints** (#684
    /// edge d): same file format and locking, keyed by catalog provider name
    /// instead of MCP server name, at `ENTANGLEMENT_LLM_TOKENS_FILE` or
    /// `${config_dir}/entanglement/llm-tokens.yml`. A separate file — not a
    /// namespace inside `mcp-tokens.yml` — so neither surface's writes ever
    /// contend with (or quarantine) the other's credentials.
    pub fn load_llm() -> Self {
        Self {
            path: llm_tokens_file_path(),
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

    /// A store with no backing file — reads empty, refuses writes. Lets a
    /// caller outside this module (e.g. `inspect::mcp`'s render tests) build a
    /// [`McpTokenStore`] without touching the process-global
    /// `ENTANGLEMENT_MCP_TOKENS_FILE` env var or the real filesystem.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self { path: None }
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
            let mut on_disk = read_tokens_for_write(&path)?;
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
            let mut on_disk = read_tokens_for_write(&path)?;
            if on_disk.remove(server).is_none() {
                return Ok(());
            }
            persist_map(&path, &on_disk)
        })
    }

    /// Take the lock **once** for the whole load-check-refresh-save sequence
    /// (#631), reusing the read/persist helpers directly rather than calling
    /// through `save` — nesting two `with_locked_file` calls on the same path
    /// from the same process would deadlock, since `fd_lock`'s guard is scoped
    /// to one open file description, not reentrant even within a single
    /// process.
    fn with_exclusive(
        &self,
        server: &str,
        f: Box<dyn FnOnce(Option<StoredAuth>) -> Result<StoredAuth> + '_>,
    ) -> Result<StoredAuth> {
        let path = self.require_path()?;
        super::lock::with_locked_file(&path, || {
            let mut on_disk = read_tokens_for_write(&path)?;
            let current = on_disk.get(server).cloned();
            let updated = f(current)?;
            on_disk.insert(server.to_string(), updated.clone());
            persist_map(&path, &on_disk)?;
            Ok(updated)
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

/// The outcome of trying to load the on-disk file, before any policy decision
/// (fail-open vs quarantine-and-fail-open) is applied to it.
enum RawRead {
    /// No file yet.
    Missing,
    Parsed(BTreeMap<String, StoredAuth>),
    /// Read or parse failed; carries serde's own error string (a line/column
    /// and field name, never a value — safe to log, unlike the file body).
    Corrupt(String),
}

fn read_raw(path: &Path) -> RawRead {
    if !path.exists() {
        return RawRead::Missing;
    }
    match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_yaml::from_str::<McpTokensFile>(&t).map_err(|e| e.to_string()))
    {
        Ok(file) => RawRead::Parsed(file.servers),
        Err(e) => RawRead::Corrupt(e),
    }
}

/// Read + parse the token file for a *read-only* query (`servers`/`has`/
/// `load`). A missing file, or any read/parse error, yields an empty map —
/// fail-open, since a read can't destroy anything.
fn read_tokens(path: &Path) -> BTreeMap<String, StoredAuth> {
    match read_raw(path) {
        RawRead::Missing => BTreeMap::new(),
        RawRead::Parsed(servers) => servers,
        RawRead::Corrupt(e) => {
            tracing::warn!(
                path = %path.display(),
                "mcp-tokens: could not parse the managed token file ({e}); treating as empty — \
                 re-run `/mcp connect <server>` to re-authorize"
            );
            BTreeMap::new()
        }
    }
}

/// Read + parse the token file for a *write* (`save`/`delete`), which is about
/// to read-modify-write the file back out. Treating a corrupt file as an empty
/// map here — like the read path does — would have the next write silently
/// erase every other server's credential (#549). So a corrupt file is instead
/// moved aside first; only once it's safely out of the way does the write
/// proceed from an empty map. If the move itself fails, the write bails
/// entirely rather than risk clobbering possibly-recoverable content.
fn read_tokens_for_write(path: &Path) -> Result<BTreeMap<String, StoredAuth>> {
    match read_raw(path) {
        RawRead::Missing => Ok(BTreeMap::new()),
        RawRead::Parsed(servers) => Ok(servers),
        RawRead::Corrupt(e) => {
            quarantine(path, &e)?;
            Ok(BTreeMap::new())
        }
    }
}

/// Move an unparseable managed file aside to a `.corrupt-<unix-secs>` sibling
/// so the next write starts from empty without destroying the original bytes.
fn quarantine(path: &Path, parse_err: &str) -> Result<()> {
    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut quarantine_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "mcp-tokens.yml".into());
    quarantine_name.push(format!(".corrupt-{epoch_secs}"));
    let quarantine_path = path.with_file_name(quarantine_name);

    std::fs::rename(path, &quarantine_path).with_context(|| {
        format!(
            "mcp-tokens: {} is unparseable ({parse_err}) and could not be moved aside to {}; \
             refusing to overwrite it",
            path.display(),
            quarantine_path.display()
        )
    })?;
    tracing::error!(
        path = %path.display(),
        quarantine_path = %quarantine_path.display(),
        "mcp-tokens: managed file was unparseable ({parse_err}); moved aside rather than \
         overwriting it, and starting a fresh empty store — re-run `/mcp connect <server>` \
         for each server, or inspect the moved-aside file by hand to recover a refresh token"
    );
    Ok(())
}

fn tokens_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(MCP_TOKENS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("mcp-tokens.yml"))
}

fn llm_tokens_file_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(LLM_TOKENS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("llm-tokens.yml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::TokenSet;

    /// `ENTANGLEMENT_MCP_TOKENS_FILE` is process-global; tests that set it
    /// serialize here so they don't race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The LLM sibling store (#684 edge d) resolves its own env var and file,
    /// fully independent of the MCP one.
    #[test]
    fn llm_store_resolves_its_own_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = std::env::temp_dir().join(format!(
            "entanglement-llm-tokens-{}.yml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::env::set_var(LLM_TOKENS_FILE_ENV, &path);
        let llm = McpTokenStore::load_llm();
        std::env::remove_var(LLM_TOKENS_FILE_ENV);
        assert_eq!(llm.path.as_deref(), Some(path.as_path()));
        // Default resolution ends in the sibling file name, never the MCP one.
        if let Some(default_path) = super::llm_tokens_file_path() {
            assert!(default_path.ends_with("entanglement/llm-tokens.yml"));
        }
        let _ = std::fs::remove_file(&path);
    }

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
    fn save_over_a_corrupt_file_quarantines_it_instead_of_overwriting() {
        let (store, path) = store_at("quarantine");
        let garbage = "servers: [this is not a map]\n";
        std::fs::write(&path, garbage).unwrap();

        // #549: this must NOT silently discard `garbage` — it must move it
        // aside so it's still recoverable by hand.
        store.save("srv", &auth("fresh")).unwrap();

        // The live file now holds only what was just saved.
        assert_eq!(
            store.load("srv").unwrap().unwrap().tokens.access_token,
            "fresh"
        );

        // The original corrupt bytes survive in a quarantined sibling.
        let dir = path.parent().unwrap();
        let stem = path.file_name().unwrap().to_string_lossy().into_owned();
        let quarantined: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&format!("{stem}.corrupt-")))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            quarantined.len(),
            1,
            "expected exactly one quarantined file"
        );
        assert_eq!(std::fs::read_to_string(&quarantined[0]).unwrap(), garbage);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&quarantined[0]);
    }

    #[test]
    fn unknown_top_level_key_is_forward_compat_not_a_wipe() {
        let (store, path) = store_at("unknown-key");
        // A newer skutter version (or a stray top-level key) must not turn
        // into a parse error that then wipes the file on the next write
        // (#549) — dropping `deny_unknown_fields` keeps this readable.
        std::fs::write(
            &path,
            "servers:\n  srv:\n    client_id: cid\n    token_endpoint: https://as.example/token\n    tokens:\n      access_token: tok\nfuture_top_level_field: 1\n",
        )
        .unwrap();
        assert_eq!(store.servers(), vec!["srv".to_string()]);
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

    /// Mirrors `config::lock::serializes_concurrent_critical_sections`: several
    /// OS threads race `with_exclusive` on the *same* server in the *same*
    /// file, each doing a read-then-increment-then-save of a counter folded
    /// into `tokens.access_token`. A lock-once refactor that reintroduced a
    /// torn read-modify-write would lose increments under contention; a
    /// refactor that (incorrectly) nested a second `with_locked_file` call
    /// inside this one would deadlock the whole test instead of merely
    /// failing an assertion — this is a plain `std::thread` test (like
    /// `lock.rs`'s) specifically so that failure mode is visible as a hang
    /// rather than swallowed by an async runtime.
    #[test]
    fn with_exclusive_serializes_across_threads_without_losing_updates_or_deadlocking() {
        let (store, path) = store_at("with-exclusive-concurrency");
        store.save("srv", &auth("0")).unwrap();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    store
                        .with_exclusive(
                            "srv",
                            Box::new(|current| {
                                let mut current = current.expect("seeded above");
                                let count: u32 = current.tokens.access_token.parse().unwrap_or(0);
                                current.tokens.access_token = (count + 1).to_string();
                                Ok(current)
                            }),
                        )
                        .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let final_count: u32 = store
            .load("srv")
            .unwrap()
            .unwrap()
            .tokens
            .access_token
            .parse()
            .unwrap();
        assert_eq!(final_count, 8 * 25);
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
