//! The last-seen MCP tool-name cache (ADR-0226): `<state dir>/mcp-tools.yml`.
//!
//! Nothing connects at startup any more, and MCP has no offline catalog —
//! `tools/list` needs a live session — so `list_mcp` would otherwise show a
//! never-connected server with no tools at all. Every successful global
//! connect records the server's tool names here; `list_mcp` shows them as
//! "last seen" until the server connects again. Keyed by server name **and**
//! transport target, so repointing a name at another endpoint never serves the
//! old endpoint's list. Per-user scoped servers (ADR-0188) are never recorded:
//! the same target may expose different tools per credential.
//!
//! Managed, not hand-edited: missing/malformed reads as empty, a write failure
//! is logged by the caller and never fatal.
//!
//! ```yaml
//! servers:
//!   web_search_prime:
//!     target: https://api.z.ai/api/mcp/web_search_prime/mcp
//!     tools: [webSearchPrime]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::atomic::atomic_write;
use crate::config::lock::with_locked_file;
use crate::config::state_dir::{state_file, MCP_TOOLS_FILE};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheFile {
    #[serde(default)]
    servers: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    target: String,
    tools: Vec<String>,
}

/// Handle on the cache file; `None` path (no config dir) makes every read
/// empty and every write a no-op.
#[derive(Debug, Clone, Default)]
pub struct McpToolCache {
    path: Option<PathBuf>,
}

impl McpToolCache {
    /// The managed file in the state dir (`ENTANGLEMENT_STATE_DIR`).
    pub fn managed() -> Self {
        Self::at(state_file(MCP_TOOLS_FILE))
    }

    /// An explicit file (tests), or `None` for a purely inert cache.
    pub fn at(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    /// The tool names last seen on `server` at `target`, `None` when never
    /// recorded or recorded for a different target.
    pub fn lookup(&self, server: &str, target: &str) -> Option<Vec<String>> {
        let path = self.path.as_deref()?;
        read(path)
            .servers
            .remove(server)
            .filter(|e| e.target == target)
            .map(|e| e.tools)
    }

    /// Record `tools` as `server`'s list at `target`, merged into the file
    /// under its lock so a concurrent instance's entries survive.
    pub fn record(&self, server: &str, target: &str, tools: &[String]) -> Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        with_locked_file(path, || {
            let mut file = read(path);
            file.servers.insert(
                server.to_string(),
                Entry {
                    target: target.to_string(),
                    tools: tools.to_vec(),
                },
            );
            atomic_write(path, &serde_yaml::to_string(&file)?)
        })
    }
}

fn read(path: &Path) -> CacheFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return CacheFile::default();
    };
    serde_yaml::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!("{}: ignoring malformed MCP tool cache: {e}", path.display());
        CacheFile::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_a_changed_target_misses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = McpToolCache::at(Some(dir.path().join("mcp-tools.yml")));
        assert_eq!(cache.lookup("gh", "https://a"), None);
        cache.record("gh", "https://a", &["search".into()]).unwrap();
        cache
            .record("kb", "kb-server --x", &["read".into()])
            .unwrap();
        assert_eq!(cache.lookup("gh", "https://a"), Some(vec!["search".into()]));
        assert_eq!(
            cache.lookup("gh", "https://b"),
            None,
            "target is part of the key"
        );
        assert_eq!(
            cache.lookup("kb", "kb-server --x"),
            Some(vec!["read".into()])
        );
    }

    #[test]
    fn a_malformed_file_reads_empty_and_an_inert_cache_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-tools.yml");
        std::fs::write(&path, "servers: [nope").unwrap();
        assert_eq!(McpToolCache::at(Some(path)).lookup("gh", "x"), None);
        assert!(McpToolCache::at(None).record("gh", "x", &[]).is_ok());
    }
}
