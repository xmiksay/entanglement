//! The MCP roster's storage seams and their in-memory defaults (#966).
//!
//! The library never touches the filesystem for MCP state: OAuth credentials
//! reach the transport only through a [`TokenStore`], and connects record
//! their tool names through an [`McpToolCache`]. Both default to memory, so a
//! credential or a last-seen list lasts as long as the process. `skutter`
//! plugs in its managed `mcp-tokens.yml` / `mcp-tools.yml` files instead.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use entanglement_core::{StoredAuth, TokenStore};

/// The last-seen MCP tool names (ADR-0226): nothing connects at startup and
/// `tools/list` needs a live session, so `list_mcp` shows what a server
/// exposed the last time it connected. Keyed by server name **and** transport
/// target, so a name repointed at another endpoint never serves the old list.
/// Per-user scoped servers (ADR-0188) are never recorded: the same target may
/// expose different tools per credential.
pub trait McpToolCache: Send + Sync {
    /// The tool names last seen on `server` at `target`; `None` when never
    /// recorded, or recorded for a different target.
    fn lookup(&self, server: &str, target: &str) -> Option<Vec<String>>;
    /// Record `tools` as `server`'s list at `target`. May block (a file-backed
    /// cache takes a lock); callers run it off the async executor.
    fn record(&self, server: &str, target: &str, tools: &[String]) -> Result<()>;
}

/// The default [`McpToolCache`]: one process's connects.
#[derive(Default)]
pub struct InMemoryMcpToolCache {
    servers: Mutex<HashMap<String, (String, Vec<String>)>>,
}

impl InMemoryMcpToolCache {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, (String, Vec<String>)>> {
        self.servers.lock().expect("MCP tool cache mutex poisoned")
    }
}

impl McpToolCache for InMemoryMcpToolCache {
    fn lookup(&self, server: &str, target: &str) -> Option<Vec<String>> {
        self.lock()
            .get(server)
            .filter(|(seen_at, _)| seen_at == target)
            .map(|(_, tools)| tools.clone())
    }

    fn record(&self, server: &str, target: &str, tools: &[String]) -> Result<()> {
        self.lock()
            .insert(server.to_string(), (target.to_string(), tools.to_vec()));
        Ok(())
    }
}

/// The default [`TokenStore`]: credentials `/mcp connect` obtained last until
/// the process exits.
#[derive(Default)]
pub struct InMemoryTokenStore {
    servers: Mutex<HashMap<String, StoredAuth>>,
}

impl InMemoryTokenStore {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, StoredAuth>> {
        self.servers.lock().expect("MCP token store mutex poisoned")
    }
}

impl TokenStore for InMemoryTokenStore {
    fn load(&self, server: &str) -> Result<Option<StoredAuth>> {
        Ok(self.lock().get(server).cloned())
    }

    fn save(&self, server: &str, auth: &StoredAuth) -> Result<()> {
        self.lock().insert(server.to_string(), auth.clone());
        Ok(())
    }

    fn delete(&self, server: &str) -> Result<()> {
        self.lock().remove(server);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::TokenSet;

    #[test]
    fn the_tool_cache_round_trips_and_a_changed_target_misses() {
        let cache = InMemoryMcpToolCache::default();
        assert_eq!(cache.lookup("gh", "https://a"), None);
        cache.record("gh", "https://a", &["search".into()]).unwrap();
        assert_eq!(cache.lookup("gh", "https://a"), Some(vec!["search".into()]));
        assert_eq!(
            cache.lookup("gh", "https://b"),
            None,
            "target is part of the key"
        );
    }

    #[test]
    fn the_token_store_saves_loads_and_deletes() {
        let store = InMemoryTokenStore::default();
        let auth = StoredAuth {
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
        };
        assert!(store.load("kb").unwrap().is_none());
        store.save("kb", &auth).unwrap();
        assert_eq!(store.load("kb").unwrap().unwrap().client_id, "cid");
        store.delete("kb").unwrap();
        store.delete("kb").unwrap();
        assert!(store.load("kb").unwrap().is_none());
    }
}
