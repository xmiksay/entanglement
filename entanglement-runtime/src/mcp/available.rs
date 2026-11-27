//! The MCP server roster (ADR-0226: registered = available).
//!
//! The user's `mcp:` entries feed it. Every entry not `disabled: true` is
//! available to every session; nothing connects at startup. The first
//! `mcp_tools` read or `mcp(...)` call on a server connects it for the process
//! (`available_enable`), and each call is still graded by the session's
//! permission mode.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use entanglement_core::TokenStore;
use tokio::sync::Mutex as AsyncMutex;

use super::{InMemoryMcpToolCache, InMemoryTokenStore, McpServerConfig, McpToolCache};

#[path = "available_enable.rs"]
mod enable;
pub use enable::{ensure_connected, resolve_unregistered, try_lazy_connect, LazyConnectOutcome};

#[path = "available_tier.rs"]
mod tier;
pub use tier::{
    disabled_decline, missing_tool_decline, server_name_of, unknown_server_decline, McpTier,
};

/// The available-server roster, shared (`Arc`) by the MCP responder (`/mcp`
/// list/add/remove), the discovery tools and dispatch's lazy connect.
pub struct AvailableMcp {
    /// Mutable so a live `/mcp add`/`remove` keeps "registered = available"
    /// true without a restart.
    servers: RwLock<HashMap<String, McpServerConfig>>,
    /// The catalog's provider key envs, scrubbed from any stdio child (#164).
    secret_env: Vec<String>,
    /// Per-server async guard serializing the check-then-connect-then-register
    /// sequence (#556): without it two concurrent first calls both connect and
    /// orphan the loser's registered tools and live connection.
    connecting: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Servers explicitly configured `disabled` (ADR-0201, `tier.rs`) — kept
    /// so "disabled" is distinguishable from "never configured" at dispatch.
    disabled_names: HashSet<String>,
    /// Last failed dispatch-time lazy connect per server (ADR-0201, `tier.rs`).
    recent_connect_failures: Mutex<HashMap<String, Instant>>,
    /// The detached script-triggered connect per server (#831).
    script_connects: super::InFlight,
    /// Where connects record their tool names for `list_mcp`'s "last seen".
    tool_cache: Arc<dyn McpToolCache>,
    /// The OAuth credential store (ADR-0153): `/mcp connect` saves into it,
    /// a global connect reads its bearer from it, and an `oauth:` server's
    /// "auth required" state checks it.
    tokens: Arc<dyn TokenStore>,
    /// Startup clash notices ([`clash_warnings`](Self::clash_warnings)).
    clash_warnings: Vec<String>,
}

/// Both stores default to memory (#966): nothing outlives the process unless
/// the head plugs in its own ([`with_token_store`](AvailableMcp::with_token_store),
/// [`with_tool_cache`](AvailableMcp::with_tool_cache)).
impl Default for AvailableMcp {
    fn default() -> Self {
        Self {
            servers: RwLock::default(),
            secret_env: Vec::new(),
            connecting: Mutex::default(),
            disabled_names: HashSet::new(),
            recent_connect_failures: Mutex::default(),
            script_connects: super::InFlight::default(),
            tool_cache: Arc::new(InMemoryMcpToolCache::default()),
            tokens: Arc::new(InMemoryTokenStore::default()),
            clash_warnings: Vec::new(),
        }
    }
}

impl AvailableMcp {
    /// Build the roster from the user's `mcp:` map. `disabled` entries are
    /// left out (and remembered as such).
    pub fn from_config(
        user_mcp: &HashMap<String, McpServerConfig>,
        secret_env: Vec<String>,
    ) -> AvailableMcp {
        // Deterministic order, by name, so which of two clashing names wins
        // never depends on hash order.
        let mut candidates: Vec<_> = user_mcp.iter().collect();
        candidates.sort_by(|a, b| a.0.cmp(b.0));
        let (disabled, enabled): (Vec<_>, Vec<_>) =
            candidates.into_iter().partition(|(_, cfg)| cfg.disabled);
        let mut servers = HashMap::new();
        let mut claimed: Vec<String> = Vec::new();
        let mut clash_warnings = Vec::new();
        // Only a server that can register tools claims its name: a disabled
        // one must not knock out a clashing server that would.
        for (name, cfg) in enabled {
            if let Some(other) = claimed.iter().find(|k| super::tool::names_clash(k, name)) {
                clash_warnings.push(clash_warning(name, other));
                continue;
            }
            claimed.push(name.clone());
            servers.insert(name.clone(), cfg.clone());
        }
        // A disabled name clashing with a claimed one stays out of
        // `disabled_names` too, so dispatch's longest-prefix routing
        // (`server_for_tool`) still sends its tools to the claimant.
        let disabled_names = disabled
            .into_iter()
            .filter(|(name, _)| !claimed.iter().any(|k| super::tool::names_clash(k, name)))
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        AvailableMcp {
            servers: RwLock::new(servers),
            secret_env,
            disabled_names,
            clash_warnings,
            ..Default::default()
        }
    }

    /// An available server whose tool names `name`'s would collide with —
    /// see [`names_clash`](super::tool::names_clash). A live `/mcp add` is refused on one.
    pub fn clashing_name(&self, name: &str) -> Option<String> {
        self.available_names()
            .into_iter()
            .find(|other| other != name && super::tool::names_clash(other, name))
    }

    /// One line per configured server [`from_config`](Self::from_config)
    /// ignored because its tool names clash with an earlier one's — for the
    /// head to show with the other config warnings.
    pub fn clash_warnings(&self) -> &[String] {
        &self.clash_warnings
    }

    /// Record connects into `cache` instead of memory (`skutter` passes its
    /// managed `mcp-tools.yml`).
    pub fn with_tool_cache(mut self, cache: Arc<dyn McpToolCache>) -> Self {
        self.tool_cache = cache;
        self
    }

    /// Keep OAuth credentials in `tokens` instead of memory (`skutter` passes
    /// its managed `mcp-tokens.yml`).
    pub fn with_token_store(mut self, tokens: Arc<dyn TokenStore>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Whether `name` declares `oauth:` but has no stored credential yet —
    /// a normal, recoverable state (`/mcp connect <name>`), checked before
    /// any connect so it never surfaces as a transport failure. A store that
    /// fails to load counts as no credential.
    pub fn needs_auth(&self, name: &str, cfg: &McpServerConfig) -> bool {
        cfg.oauth.is_some() && !matches!(self.tokens.load(name), Ok(Some(_)))
    }

    /// The OAuth credential store.
    pub fn token_store(&self) -> Arc<dyn TokenStore> {
        self.tokens.clone()
    }

    /// The last-seen tool-name cache connects record into.
    pub fn tool_cache(&self) -> &Arc<dyn McpToolCache> {
        &self.tool_cache
    }

    /// Every available server, sorted by name.
    pub fn available_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.read().keys().cloned().collect();
        names.sort();
        names
    }

    /// Look up an available server's config by name (`None` when unknown or
    /// `disabled`).
    pub fn get(&self, name: &str) -> Option<McpServerConfig> {
        self.read().get(name).cloned()
    }

    /// Register a live-added user server (`/mcp add`).
    pub fn insert_user(&self, name: &str, config: McpServerConfig) {
        self.write().insert(name.to_string(), config);
    }

    /// Withdraw a live-removed server (`/mcp remove`).
    pub fn remove(&self, name: &str) {
        self.write().remove(name);
    }

    pub(crate) fn secret_env(&self) -> &[String] {
        &self.secret_env
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, McpServerConfig>> {
        self.servers.read().expect("MCP roster lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, McpServerConfig>> {
        self.servers.write().expect("MCP roster lock poisoned")
    }

    /// The per-server async guard for [`ensure_connected`]'s connect section
    /// (#556) — the same `Arc<AsyncMutex<()>>` for every caller naming the
    /// same `server`, minted on first use.
    fn connect_guard(&self, server: &str) -> Arc<AsyncMutex<()>> {
        self.connecting
            .lock()
            .expect("MCP connect-guard mutex poisoned")
            .entry(server.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub(crate) fn script_connects(&self) -> &super::InFlight {
        &self.script_connects
    }
}

fn clash_warning(name: &str, other: &str) -> String {
    format!(
        "MCP server `{name}` ignored: its tool names would collide with server `{other}`'s \
         (`{}` vs `{}`) — rename one",
        super::tool::namespaced_tool_name(other, ""),
        super::tool::namespaced_tool_name(name, "")
    )
}

#[cfg(test)]
#[path = "available_tests.rs"]
pub(crate) mod tests;
