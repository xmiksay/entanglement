//! Available (state `allowed`) MCP servers + per-session enablement (#542).
//!
//! Two sources feed the *available* set: **provider-bundled** servers from the
//! catalog (`ProviderEntry::mcp_servers` — e.g. z.ai's `web_search_prime`/
//! `web_reader`/`zread`, unlocked by the provider's `key_env`) and **user
//! `mcp:` entries** whose `state` is `allowed`. An available server is not
//! connected and its tools don't exist — until a user (`/enable mcp <name>`)
//! or the agent itself (the `mcp_enable` tool) enables it, which lazily
//! connects it and marks it visible **for that session only**: the runtime's
//! `tool_spec_resolver` filters a lazily-connected server's specs to its
//! enabling sessions, so enablement is session-ephemeral — nothing persists,
//! and `ServerConfigs`/`save_mcp` never see a bundled server (durable state
//! changes are config edits, by design).
//!
//! Key gating is read **live** from the process env on every availability
//! check, so a `/key` save (which `set_var`s the key) unlocks a provider's
//! bundle with no restart; a keyless bundle is silently absent everywhere.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::sync::Mutex as AsyncMutex;
// Catalog types come via core's re-export (ADR-0053): the runtime's direct
// `entanglement-provider` dep is optional (`provider` feature) and absent
// from the lean build, but core's unconditional dep carries them everywhere.
use entanglement_core::{Catalog, McpServerState, ProviderMcpServer, SessionId};

use crate::tools::SharedRegistry;

use super::live::{ActiveServer, ActiveServers};
use super::{connect_client, register_tools, transport_label, McpServerConfig};

/// Ceiling on a lazy `mcp_enable` connect (#556): a hung server must not park
/// the calling turn — nor every other caller waiting on the per-server
/// [`AvailableMcp::connect_guard`] — forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

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
}

impl AvailableMcp {
    /// Split the effective server universe — catalog bundles overlaid with the
    /// user's `mcp:` map — into the startup-connect set (state `enabled`;
    /// bundled ones only when their key resolves at startup) and the
    /// [`AvailableMcp`] roster (state `allowed`). `disabled` entries land in
    /// neither. The user's own map is returned untouched elsewhere
    /// (`ServerConfigs`) — bundled servers never join the persistence set.
    pub fn partition(
        catalog: &Catalog,
        user_mcp: &HashMap<String, McpServerConfig>,
        secret_env: Vec<String>,
    ) -> (HashMap<String, McpServerConfig>, AvailableMcp) {
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
                        startup.insert(name.clone(), entry.config);
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
                    startup.insert(name.clone(), cfg.clone());
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
    /// a lazily-connected server is visible only to sessions that enabled it;
    /// everything else (host tools, startup-connected servers) passes.
    pub fn spec_visible(&self, tool_name: &str, session: &SessionId) -> bool {
        let Some(server) = tool_name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split("__").next())
        else {
            return true;
        };
        match self.enabled.lock().unwrap().get(server) {
            Some(sessions) => sessions.contains(session),
            None => true,
        }
    }

    /// Mark `server` enabled for `session` (idempotent).
    pub fn mark_enabled(&self, server: &str, session: &SessionId) {
        self.enabled
            .lock()
            .unwrap()
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
        let mut enabled = self.enabled.lock().unwrap();
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
        self.enabled.lock().unwrap().contains_key(server)
    }

    /// The per-server async guard for [`enable_for_session`]'s connect
    /// section (#556) — the same `Arc<AsyncMutex<()>>` for every caller
    /// naming the same `server`, minted on first use.
    fn connect_guard(&self, server: &str) -> Arc<AsyncMutex<()>> {
        self.connecting
            .lock()
            .unwrap()
            .entry(server.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

/// Enable an available server for `session` (#542): lazily connect + register
/// its tools if this is the first enablement (no persistence — `ServerConfigs`
/// and `config.yml` are never touched, unlike `mcp_add`), then mark the
/// session. Returns the server's registered tool names.
pub async fn enable_for_session(
    avail: &AvailableMcp,
    name: &str,
    session: &SessionId,
    registry: &SharedRegistry,
    active: &ActiveServers,
) -> Result<Vec<String>> {
    let connected = active.lock().unwrap().get(name).map(|s| s.tools.clone());
    // A startup-`enabled` server is globally visible already — enabling it
    // must *not* mark it lazy, or the spec filter would suddenly scope it to
    // this session and hide it everywhere else.
    if !avail.is_lazy(name) && avail.get(name).is_none() {
        return match connected {
            Some(tools) => Ok(tools),
            None => bail!(
                "no available MCP server named `{name}` (available: {})",
                avail.available_names().join(", ")
            ),
        };
    }
    let tools = match connected {
        Some(tools) => tools,
        None => {
            // Serialize the check-then-connect-then-register sequence per
            // server (#556 TOCTOU): two concurrent enables of a not-yet-
            // connected server must not both pass the check above,
            // double-connect, and orphan the loser's registered tools.
            let guard = avail.connect_guard(name);
            let _permit = guard.lock().await;
            // Re-check now that we hold the guard — another caller may have
            // finished connecting while we waited for it. Bound to a `let`
            // first so the `MutexGuard` drops before the `else` branch's
            // `.await`s (an `if let`'s scrutinee temporary otherwise lives
            // for the whole if/else, poisoning the future's `Send`-ness).
            let already_connected = active.lock().unwrap().get(name).map(|s| s.tools.clone());
            if let Some(tools) = already_connected {
                tools
            } else {
                let Some(server) = avail.get(name) else {
                    bail!(
                        "no available MCP server named `{name}` (available: {})",
                        avail.available_names().join(", ")
                    );
                };
                let (client, defs) = match tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    connect_client(name, &server.config, &avail.secret_env),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => bail!("connecting MCP server `{name}` timed out"),
                };
                let tools = {
                    let mut reg = registry.write().unwrap();
                    reg.unregister_prefix(&format!("mcp__{name}__"));
                    register_tools(&mut reg, &client, name, defs)
                };
                active.lock().unwrap().insert(
                    name.to_string(),
                    ActiveServer {
                        client,
                        tools: tools.clone(),
                        transport: transport_label(&server.config),
                    },
                );
                tracing::info!(
                    "MCP server `{name}`: lazily connected, {} tool(s)",
                    tools.len()
                );
                tools
            }
        }
    };
    avail.mark_enabled(name, session);
    Ok(tools)
}

/// Disconnect a lazily-connected available server entirely (the `/mcp remove`
/// arm for a bundled name): unregister its tools, drop the connection, clear
/// every session's enablement mark. Nothing is persisted — the server stays
/// *available* and can be re-enabled.
pub fn disconnect(
    avail: &AvailableMcp,
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
) {
    registry
        .write()
        .unwrap()
        .unregister_prefix(&format!("mcp__{name}__"));
    active.lock().unwrap().remove(name);
    avail.enabled.lock().unwrap().remove(name);
    tracing::info!("MCP server `{name}`: disconnected (still available)");
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
