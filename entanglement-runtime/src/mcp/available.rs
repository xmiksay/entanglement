//! The MCP server roster (ADR-0226: registered = available).
//!
//! Two sources feed it: **provider-bundled** servers from the catalog
//! (`ProviderEntry::mcp_servers` — e.g. z.ai's `web_search_prime`/
//! `web_reader`/`zread`, unlocked by the provider's `key_env`) and the user's
//! `mcp:` entries. Every entry not `disabled: true` is available to every
//! session; nothing connects at startup. The first `mcp_tools` read or
//! `mcp(...)` call on a server connects it for the process (`available_enable`),
//! and each call is still graded by the session's permission mode.
//!
//! Key gating is read **live** from the process env on every lookup, so a
//! `/key` save (which `set_var`s the key) unlocks a provider's bundle with no
//! restart; a keyless bundle is silently absent everywhere.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use tokio::sync::Mutex as AsyncMutex;
// Catalog types come via core's re-export (ADR-0053): the runtime's direct
// `entanglement-provider` dep is optional (`provider` feature) and absent
// from the lean build, but core's unconditional dep carries them everywhere.
use entanglement_core::{Catalog, ProviderMcpServer};

use super::{McpServerConfig, McpToolCache};
use crate::config::McpTokenStore;

#[path = "available_enable.rs"]
mod enable;
pub use enable::{
    disconnect, ensure_connected, resolve_unregistered, try_lazy_connect, LazyConnectOutcome,
};

#[path = "available_catalog.rs"]
mod catalog;
pub use catalog::bundled_capability_configs;

#[path = "available_tier.rs"]
mod tier;
pub use tier::{
    disabled_decline, missing_tool_decline, server_name_of, unknown_server_decline, McpTier,
};

/// One available server: its resolved config (bundled definition field-merged
/// with any same-name user `mcp:` override), the env var gating its
/// availability (`None` ⇒ ungated, a user-declared entry), and the bundling
/// provider for display.
#[derive(Clone)]
pub struct AvailableServer {
    pub config: McpServerConfig,
    pub key_env: Option<String>,
    pub provider: Option<String>,
}

impl AvailableServer {
    /// Availability is key presence, checked live (env or the managed `.env`
    /// already loaded into the process env; a TUI `/key` save `set_var`s it).
    pub fn key_ok(&self) -> bool {
        match &self.key_env {
            None => true,
            Some(var) => std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false),
        }
    }

    fn user(config: McpServerConfig) -> Self {
        Self {
            config,
            key_env: None,
            provider: None,
        }
    }
}

/// The available-server roster, shared (`Arc`) by the MCP responder (`/mcp`
/// list/add/remove), the discovery tools and dispatch's lazy connect.
#[derive(Default)]
pub struct AvailableMcp {
    /// Mutable so a live `/mcp add`/`remove` keeps "registered = available"
    /// true without a restart.
    servers: RwLock<HashMap<String, AvailableServer>>,
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
    /// Inert by default so a test never writes the managed file; the binary
    /// installs [`McpToolCache::managed`].
    tool_cache: McpToolCache,
    /// The OAuth credential store an `oauth:` server's "auth required" state
    /// reads (ADR-0153). `None` — the default — is the managed
    /// `mcp-tokens.yml`, resolved only when an `oauth:` server is checked, so
    /// an embedder never sees "auth required" forever for a credential
    /// `/mcp connect` stored.
    tokens: Option<McpTokenStore>,
    /// Startup clash notices ([`clash_warnings`](Self::clash_warnings)).
    clash_warnings: Vec<String>,
}

impl AvailableMcp {
    /// Build the roster from the catalog bundles overlaid with the user's
    /// `mcp:` map. `disabled` entries are left out (and remembered as such).
    /// The user's own map is kept untouched elsewhere (`ServerConfigs`) —
    /// bundled servers never join the persistence set.
    pub fn from_config(
        catalog: &Catalog,
        user_mcp: &HashMap<String, McpServerConfig>,
        secret_env: Vec<String>,
    ) -> AvailableMcp {
        // Deterministic order — bundles in catalog order, then user entries,
        // each by name — so which of two clashing names wins never depends
        // on hash order.
        let mut candidates: Vec<(String, Option<AvailableServer>)> = Vec::new();
        for provider in &catalog.providers {
            let mut bundled: Vec<_> = provider.mcp_servers.iter().collect();
            bundled.sort_by(|a, b| a.0.cmp(b.0));
            for (name, bundled) in bundled {
                let mut cfg = bundled_config(bundled);
                if let Some(user) = user_mcp.get(name) {
                    cfg = merge_user_over_bundled(cfg, user);
                }
                let entry = (!cfg.disabled).then(|| AvailableServer {
                    config: cfg,
                    key_env: provider.key_env.clone(),
                    provider: Some(provider.name.clone()),
                });
                candidates.push((name.clone(), entry));
            }
        }
        let mut user: Vec<_> = user_mcp.iter().collect();
        user.sort_by(|a, b| a.0.cmp(b.0));
        for (name, cfg) in user {
            if candidates.iter().any(|(n, _)| n == name) {
                continue; // already folded in as an override above
            }
            let entry = (!cfg.disabled).then(|| AvailableServer::user(cfg.clone()));
            candidates.push((name.clone(), entry));
        }
        let mut servers = HashMap::new();
        let mut disabled_names = HashSet::new();
        let mut claimed: Vec<String> = Vec::new();
        let mut clash_warnings = Vec::new();
        // Only a server that can connect now claims its name: a disabled or
        // keyless one never registers a tool, so it must not knock out a
        // clashing server that would.
        let (connectable, inert): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|(_, entry)| entry.as_ref().is_some_and(AvailableServer::key_ok));
        for (name, entry) in connectable {
            if let Some(other) = claimed.iter().find(|k| super::tool::names_clash(k, &name)) {
                clash_warnings.push(clash_warning(&name, other));
                continue;
            }
            claimed.push(name.clone());
            servers.extend(entry.map(|entry| (name, entry)));
        }
        for (name, entry) in inert {
            if claimed.iter().any(|k| super::tool::names_clash(k, &name)) {
                continue;
            }
            match entry {
                Some(keyless) => {
                    servers.insert(name, keyless);
                }
                None => {
                    disabled_names.insert(name);
                }
            }
        }
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

    /// Record connects into `cache` (the binary passes the managed file).
    pub fn with_tool_cache(mut self, cache: McpToolCache) -> Self {
        self.tool_cache = cache;
        self
    }

    /// Read OAuth credentials from `tokens` instead of the managed store
    /// (tests pin an explicit or inert one).
    pub fn with_token_store(mut self, tokens: McpTokenStore) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Whether `name` declares `oauth:` but has no stored credential yet —
    /// a normal, recoverable state (`/mcp connect <name>`), checked before
    /// any connect so it never surfaces as a transport failure.
    pub fn needs_auth(&self, name: &str, cfg: &McpServerConfig) -> bool {
        cfg.oauth.is_some()
            && !match &self.tokens {
                Some(tokens) => tokens.has(name),
                None => McpTokenStore::load().has(name),
            }
    }

    /// The last-seen tool-name cache connects record into.
    pub fn tool_cache(&self) -> &McpToolCache {
        &self.tool_cache
    }

    /// Every available server — key resolved — sorted by name. Keyless bundles
    /// are silently absent (#542).
    pub fn available_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .read()
            .iter()
            .filter(|(_, s)| s.key_ok())
            .map(|(n, _)| n.clone())
            .collect();
        names.sort();
        names
    }

    /// Look up an available server by name (`None` when unknown, `disabled`,
    /// or its key doesn't resolve — indistinguishable by design: a keyless
    /// bundle must look absent, not locked).
    pub fn get(&self, name: &str) -> Option<AvailableServer> {
        self.read().get(name).filter(|s| s.key_ok()).cloned()
    }

    /// Register a live-added user server (`/mcp add`).
    pub fn insert_user(&self, name: &str, config: McpServerConfig) {
        self.write()
            .insert(name.to_string(), AvailableServer::user(config));
    }

    /// Withdraw a live-removed server (`/mcp remove` of a user entry). A
    /// bundled server is never removed — it stays available after a
    /// disconnect.
    pub fn remove(&self, name: &str) {
        self.write().remove(name);
    }

    /// Whether `name` is a provider-bundled server.
    pub fn is_bundled(&self, name: &str) -> bool {
        self.read().get(name).is_some_and(|s| s.provider.is_some())
    }

    pub(crate) fn secret_env(&self) -> &[String] {
        &self.secret_env
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, AvailableServer>> {
        self.servers.read().expect("MCP roster lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, AvailableServer>> {
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

/// Convert a catalog [`ProviderMcpServer`] into the runtime config shape.
fn bundled_config(b: &ProviderMcpServer) -> McpServerConfig {
    McpServerConfig {
        command: b.command.clone(),
        args: b.args.clone(),
        env: b.env.clone(),
        url: b.url.clone(),
        headers: b.headers.clone(),
        disabled: b.disabled,
        capabilities: b.capabilities.clone(),
        oauth: None,
        description: b.description.clone(),
    }
}

/// Field-wise overlay of a user `mcp:` entry onto a bundled definition (#542):
/// a field the user *set* (Some / non-empty / true) wins, everything else
/// keeps the bundled value — so `disabled: true` alone turns a bundle off
/// without re-declaring its url. Setting one transport clears the bundled
/// other, keeping the `command` XOR `url` invariant. Limitation (accepted): a
/// user cannot reset a bundled field back to empty/None, only replace it.
fn merge_user_over_bundled(mut base: McpServerConfig, user: &McpServerConfig) -> McpServerConfig {
    if user.command.is_some() {
        base.command = user.command.clone();
        base.url = None;
    }
    if user.url.is_some() {
        base.url = user.url.clone();
        base.command = None;
        base.args = Vec::new();
        base.env = HashMap::new();
    }
    if !user.args.is_empty() {
        base.args = user.args.clone();
    }
    if !user.env.is_empty() {
        base.env = user.env.clone();
    }
    if !user.headers.is_empty() {
        base.headers = user.headers.clone();
    }
    if !user.capabilities.is_empty() {
        base.capabilities = user.capabilities.clone();
    }
    // A bundled server authenticates with its provider key (a static header), so
    // the bundle never sets `oauth` — but a user may add it to point a bundled
    // URL at an OAuth-protected deployment (ADR-0153).
    if user.oauth.is_some() {
        base.oauth = user.oauth.clone();
    }
    if user.description.is_some() {
        base.description = user.description.clone();
    }
    if user.disabled {
        base.disabled = true;
    }
    base
}

#[cfg(test)]
#[path = "available_tests.rs"]
pub(crate) mod tests;
