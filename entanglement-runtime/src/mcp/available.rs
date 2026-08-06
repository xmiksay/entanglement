//! Available (state `allowed`) MCP servers + per-session enablement (#542).
//!
//! Two sources feed the *available* set: **provider-bundled** servers from the
//! catalog (`ProviderEntry::mcp_servers` — e.g. z.ai's `web_search_prime`/
//! `web_reader`/`zread`, unlocked by the provider's `key_env`) and **user
//! `mcp:` entries** whose `state` is `allowed`. An available server is not
//! connected and its tools don't exist — until a user (`/enable mcp <name>`)
//! or the agent itself (the `mcp_enable` tool) enables it, which lazily
//! connects it and marks it visible **for that session and its spawn
//! sub-tree** (#630): the runtime's `tool_spec_resolver` filters a
//! lazily-connected server's specs to its enabling sessions and their
//! descendants, so enablement is session-ephemeral — nothing persists, and
//! `ServerConfigs`/`save_mcp` never see a bundled server (durable state
//! changes are config edits, by design). Both the enablement marks and the
//! parent links feeding the descendant walk are dropped on `SessionEnded`
//! (`forget_session`), so neither grows for the process lifetime.
//!
//! Key gating is read **live** from the process env on every availability
//! check, so a `/key` save (which `set_var`s the key) unlocks a provider's
//! bundle with no restart; a keyless bundle is silently absent everywhere.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;
// Catalog types come via core's re-export (ADR-0053): the runtime's direct
// `entanglement-provider` dep is optional (`provider` feature) and absent
// from the lean build, but core's unconditional dep carries them everywhere.
use entanglement_core::{Catalog, McpServerState, ProviderMcpServer, SessionId};

use super::McpServerConfig;

#[path = "available_enable.rs"]
mod enable;
pub use enable::{disconnect, enable_for_session};

#[path = "available_lifecycle.rs"]
mod lifecycle;
pub use lifecycle::{forget_session, record_parent};

/// One available-but-not-startup-connected server: its resolved config (bundled
/// definition field-merged with any same-name user `mcp:` override), the env
/// var gating its availability (`None` ⇒ ungated, e.g. a user-declared
/// `allowed` entry), and the bundling provider for display.
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
}

/// The available-server roster plus the per-session enablement marks that
/// scope a lazily-connected server's visibility (#542). Shared (`Arc`) by the
/// TUI `/enable` path, the `mcp_enable` tool, the MCP responder (`/mcp list`)
/// and the `tool_spec_resolver` closure.
#[derive(Default)]
pub struct AvailableMcp {
    servers: HashMap<String, AvailableServer>,
    /// Lazily-connected server → the sessions that enabled it. A server key
    /// present here at all means "connected on demand, visibility scoped";
    /// startup-`enabled` servers never appear and stay globally visible.
    enabled: Mutex<HashMap<String, HashSet<SessionId>>>,
    /// The catalog's provider key envs, scrubbed from any stdio child (#164).
    secret_env: Vec<String>,
    /// Per-server async guard serializing `enable_for_session`'s
    /// check-then-connect-then-register sequence (#556): without it, two
    /// concurrent enables of the same not-yet-connected server can both pass
    /// the "not connected yet" check, both call `connect_client`, and race to
    /// insert into `active` — orphaning the loser's registered tools (and its
    /// live connection/subprocess) with nothing left holding a reference to
    /// unregister or drop them.
    connecting: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// child → parent, folded from `OutEvent::SessionStarted` (#630). Lets
    /// [`spec_visible`](Self::spec_visible) walk a session's ancestry so a
    /// spawned child inherits whichever ancestor enabled a lazy server,
    /// resolved live rather than snapshotted at spawn time — an ancestor's
    /// enable that happens *after* the child already exists is picked up too.
    /// A second, independent fold of the same `SessionStarted` broadcast
    /// `crate::subagent::SpawnGuard` already tracks: duplicated here rather
    /// than shared, since `SpawnGuard` deliberately stays single-threaded
    /// inside the tool executor's own event loop, while this map is read from
    /// the `tool_spec_resolver` closure running per-session inside core.
    parents: Mutex<HashMap<SessionId, Option<SessionId>>>,
}

impl AvailableMcp {
    /// Split the effective server universe — catalog bundles overlaid with the
    /// user's `mcp:` map — into the startup-connect set (state `enabled`;
    /// bundled ones only when their key resolves at startup) and the
    /// [`AvailableMcp`] roster (state `allowed`). `disabled` entries land in
    /// neither. The user's own map is returned untouched elsewhere
    /// (`ServerConfigs`) — bundled servers never join the persistence set.
    ///
    /// The startup set carries each entry's `key_env`/`provider` alongside its
    /// config (the same [`AvailableServer`] shape as the roster, #559) rather
    /// than a bare `McpServerConfig` — startup connect needs the bundling
    /// provider's key resolved so its traffic shares the LLM endpoint's pool
    /// key, and dropping that linkage here would leave no way to recover it.
    pub fn partition(
        catalog: &Catalog,
        user_mcp: &HashMap<String, McpServerConfig>,
        secret_env: Vec<String>,
    ) -> (HashMap<String, AvailableServer>, AvailableMcp) {
        let mut startup = HashMap::new();
        let mut servers = HashMap::new();
        let mut bundled_names = HashSet::new();
        for provider in &catalog.providers {
            for (name, bundled) in &provider.mcp_servers {
                bundled_names.insert(name.clone());
                let user = user_mcp.get(name);
                let mut cfg = bundled_config(bundled);
                if let Some(user) = user {
                    cfg = merge_user_over_bundled(cfg, user);
                }
                // A bundled server with no explicit state is `allowed`, not
                // `enabled` — the legacy-bool fallback is for user entries.
                // But a user entry colliding with a bundled name (#561) is
                // still a *user* entry: if the user set no explicit `state:`,
                // its `effective_state()` (legacy `disabled:false` ⇒
                // `Enabled`) must win over the bundled `Allowed` default, or a
                // previously startup-connected user server silently demotes
                // to lazy on the next restart.
                let state = match user {
                    Some(user) if user.state.is_none() => user.effective_state(),
                    _ => cfg.state.unwrap_or(McpServerState::Allowed),
                };
                let key_env = provider.key_env.clone();
                let entry = AvailableServer {
                    config: cfg,
                    key_env,
                    provider: Some(provider.name.clone()),
                };
                match state {
                    McpServerState::Enabled if entry.key_ok() => {
                        startup.insert(name.clone(), entry);
                    }
                    McpServerState::Enabled | McpServerState::Disabled => {}
                    McpServerState::Allowed => {
                        servers.insert(name.clone(), entry);
                    }
                }
            }
        }
        for (name, cfg) in user_mcp {
            if bundled_names.contains(name) {
                continue; // already folded in as an override above
            }
            match cfg.effective_state() {
                McpServerState::Enabled => {
                    startup.insert(
                        name.clone(),
                        AvailableServer {
                            config: cfg.clone(),
                            key_env: None,
                            provider: None,
                        },
                    );
                }
                McpServerState::Allowed => {
                    servers.insert(
                        name.clone(),
                        AvailableServer {
                            config: cfg.clone(),
                            key_env: None,
                            provider: None,
                        },
                    );
                }
                McpServerState::Disabled => {}
            }
        }
        (
            startup,
            AvailableMcp {
                servers,
                enabled: Mutex::new(HashMap::new()),
                secret_env,
                connecting: Mutex::new(HashMap::new()),
                parents: Mutex::new(HashMap::new()),
            },
        )
    }

    /// The currently *available* servers — `allowed` state and key resolved —
    /// sorted by name. Keyless bundles are silently absent (#542).
    pub fn available_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .servers
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
    pub fn get(&self, name: &str) -> Option<&AvailableServer> {
        self.servers.get(name).filter(|s| s.key_ok())
    }

    /// Whether `tool_name`'s specs are visible to `session` (#542): a tool of
    /// a lazily-connected server is visible only to sessions that enabled it
    /// or an ancestor of it (#630, `lifecycle::ancestor_enabled`) — everything
    /// else (host tools, startup-connected servers) passes.
    pub fn spec_visible(&self, tool_name: &str, session: &SessionId) -> bool {
        let Some(server) = tool_name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split("__").next())
        else {
            return true;
        };
        let enabled = self
            .enabled
            .lock()
            .expect("available-server enablement mutex poisoned");
        let Some(sessions) = enabled.get(server) else {
            return true;
        };
        self.enabled_by_or_ancestor(sessions, session)
    }

    /// Whether `session` — or an ancestor of it, live-resolved (#630) — is in
    /// `sessions`. The shared tail of [`spec_visible`](Self::spec_visible),
    /// also consulted by `crate::builtin_visibility` (ADR-0179) so lazy
    /// built-ins inherit the identical ancestor semantics without a second
    /// parent map.
    pub(crate) fn enabled_by_or_ancestor(
        &self,
        sessions: &HashSet<SessionId>,
        session: &SessionId,
    ) -> bool {
        sessions.contains(session) || lifecycle::ancestor_enabled(self, sessions, session)
    }

    /// Mark `server` enabled for `session` (idempotent).
    pub fn mark_enabled(&self, server: &str, session: &SessionId) {
        self.enabled
            .lock()
            .expect("available-server enablement mutex poisoned")
            .entry(server.to_string())
            .or_default()
            .insert(session.clone());
    }

    /// Withdraw `session`'s enablement of `server` — the symmetric,
    /// session-scoped inverse (`/disable mcp <name>`). The connection itself
    /// stays up (other sessions may still use it); the spec filter simply
    /// hides it from this session again.
    ///
    /// Drops the map entry entirely once its session set empties (#561): an
    /// entry present but empty is indistinguishable from "no sessions may see
    /// this" in [`spec_visible`](Self::spec_visible)'s `Some(sessions) ⇒
    /// sessions.contains(session)` check, so an enable→disable cycle would
    /// otherwise hide the server's tools from *every* session until restart.
    pub fn mark_disabled(&self, server: &str, session: &SessionId) {
        let mut enabled = self
            .enabled
            .lock()
            .expect("available-server enablement mutex poisoned");
        if let Some(sessions) = enabled.get_mut(server) {
            sessions.remove(session);
            if sessions.is_empty() {
                enabled.remove(server);
            }
        }
    }

    /// Whether `server` is one of the lazily-connected set (for `/mcp list`
    /// display: "allowed" vs a plain startup "enabled").
    pub fn is_lazy(&self, server: &str) -> bool {
        self.enabled
            .lock()
            .expect("available-server enablement mutex poisoned")
            .contains_key(server)
    }

    /// The per-server async guard for [`enable_for_session`]'s connect
    /// section (#556) — the same `Arc<AsyncMutex<()>>` for every caller
    /// naming the same `server`, minted on first use.
    fn connect_guard(&self, server: &str) -> Arc<AsyncMutex<()>> {
        self.connecting
            .lock()
            .expect("available-server connect-guard mutex poisoned")
            .entry(server.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

/// Convert a catalog [`ProviderMcpServer`] into the runtime config shape. The
/// bundled default state stays in `state` (resolved by `partition` — `None` ⇒
/// `allowed` there, unlike a user entry's legacy-bool fallback).
fn bundled_config(b: &ProviderMcpServer) -> McpServerConfig {
    McpServerConfig {
        command: b.command.clone(),
        args: b.args.clone(),
        env: b.env.clone(),
        url: b.url.clone(),
        headers: b.headers.clone(),
        disabled: false,
        capabilities: b.capabilities.clone(),
        oauth: None,
        state: b.state,
    }
}

/// Field-wise overlay of a user `mcp:` entry onto a bundled definition (#542):
/// a field the user *set* (Some / non-empty / true) wins, everything else
/// keeps the bundled value — so `state: disabled` alone turns a bundle off
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
    if user.disabled {
        base.disabled = true;
    }
    if user.state.is_some() {
        base.state = user.state;
    }
    base
}

#[cfg(test)]
#[path = "available_tests.rs"]
mod tests;
