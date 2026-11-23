//! Session-keyed per-user MCP scopes (#684) — the runtime consumption of
//! ADR-0184's per-user credential seam.
//!
//! A multi-user embedder gives each user their own MCP server set and OAuth
//! tokens without the provider crate's user-id type ever entering this crate
//! (ADR-0181, the grep gate):
//! it supplies an [`McpScopeResolver`] — a closure from `SessionId` to that
//! session's [`McpScope`] — and the runtime consults it at discovery
//! (`list_mcp`/`mcp_tools`) and dispatch time. The session→user mapping stays the embedder's private
//! concern; the scope's `key` is an opaque string it derives from its own user
//! identity, and the credential slice is typically
//! `entanglement_provider::user_scoped(store, user)`.
//!
//! **Replace semantics**: a scoped session's `mcp__*` namespace is entirely
//! scope-owned — `list_mcp` lists only the scope's servers, and the global
//! registry's MCP tools are stripped from its dispatch snapshots with the
//! scope's own tools in their place. That is
//! what makes same-named servers unambiguous (user A's `kb`, user B's `kb`,
//! and a global `kb` are three different endpoints behind one tool name):
//! disambiguation is structural, via the `(scope key, server)` connection
//! cache, never nominal — tool names stay `mcp__<server>__<tool>` and leak no
//! user identity to the model (ADR-0181 rejected name-spacing). An unscoped
//! session (or no resolver at all) sees exactly the global behavior.
//!
//! Connections are lazy and cached per `(scope key, server)` for the process
//! lifetime — eviction is the embedder's explicit [`McpScopes::evict_scope`]
//! call (logout, a changed server set), matching the global connections'
//! process-lifetime precedent. The resolver runs per discovery read and per
//! MCP dispatch, so it must be cheap and non-blocking — an in-memory map
//! lookup, never I/O.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::SessionId;

use crate::tools::{Tool, ToolRegistry};

use super::{McpClient, McpServerConfig};

/// One session's MCP scope, resolved by the embedder.
pub struct McpScope {
    /// Opaque cache key the embedder derives from its own user identity —
    /// sessions resolving to the same key share connections and credentials.
    pub key: String,
    /// This scope's server set — the same shape as the config `mcp:` map,
    /// including `capabilities` hints and optional `oauth:` blocks.
    pub servers: HashMap<String, McpServerConfig>,
    /// The scope's credential slice (e.g. `user_scoped(store, user)`),
    /// consulted for any server carrying an `oauth:` block. `None` with an
    /// OAuth server present means every call to it fails as auth-required.
    pub token_store: Option<Arc<dyn entanglement_core::TokenStore>>,
}

/// The embedder-supplied seam: session → scope. `None` means the session is
/// unscoped and sees the global MCP behavior unchanged. Keep it an in-memory
/// lookup.
pub type McpScopeResolver = Arc<dyn Fn(&SessionId) -> Option<McpScope> + Send + Sync>;

/// A connected per-scope server: the shared client and its registered-shape
/// tool proxies.
struct ScopeServer {
    tools: Vec<Arc<dyn Tool>>,
    /// Keeps the connection (and a stdio server's child) alive as long as the
    /// cache entry does, exactly like `ActiveServer` — the tool proxies above
    /// each hold their own `Arc<McpClient>` clone already, this field just
    /// makes the ownership explicit.
    _client: Arc<McpClient>,
}

/// The per-scope MCP connection cache + overlay entry points. Construct one
/// per embedded engine and hand it to
/// [`ToolExecutor::builder`][crate::tool_runner::ToolExecutor::builder].
pub struct McpScopes {
    resolver: McpScopeResolver,
    /// The shared endpoint pool (#559) every scope's HTTP transport rides.
    http: entanglement_core::HttpClient,
    /// Provider API-key env vars scrubbed from stdio children (#164), same as
    /// the global connect path.
    secret_env: Arc<[String]>,
    /// `(scope key, server)` → connected entry. Process-lifetime; see
    /// [`evict_scope`][Self::evict_scope].
    cache: Mutex<ScopeCache>,
    /// Per-`(scope key, server)` connect guard — the #556 double-checked
    /// pattern, so two concurrent first calls connect once.
    connecting: Mutex<ConnectGuards>,
    /// Scope key → eviction count; a connect caches its entry only if the
    /// count is unchanged since it started.
    generations: Mutex<HashMap<String, u64>>,
    /// The detached script-triggered connect per `(scope key, server)`.
    script_connects: super::InFlight,
}

/// `(scope key, server)` → connected entry.
type ScopeCache = HashMap<(String, String), Arc<ScopeServer>>;
/// `(scope key, server)` → its in-flight connect lock.
type ConnectGuards = HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

impl McpScopes {
    pub fn new(
        resolver: McpScopeResolver,
        http: entanglement_core::HttpClient,
        secret_env: Vec<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            resolver,
            http,
            secret_env: Arc::from(secret_env),
            cache: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
            script_connects: super::InFlight::default(),
        })
    }

    /// The session's scope, per the embedder's resolver.
    pub fn scope_of(&self, session: &SessionId) -> Option<McpScope> {
        (self.resolver)(session)
    }

    /// Connect every server in the session's scope, concurrently and
    /// best-effort, so `list_mcp` shows their tools before the first prompt.
    /// Returns per-server failures — empty means every server connected.
    /// Optional: `mcp_tools` and a dispatch connect a server lazily anyway.
    pub async fn prewarm(self: &Arc<Self>, session: &SessionId) -> Vec<(String, anyhow::Error)> {
        let Some(scope) = self.scope_of(session) else {
            return Vec::new();
        };
        let scope = Arc::new(scope);
        let mut pending = tokio::task::JoinSet::new();
        let (usable, skipped) = usable_servers(&scope);
        for (name, other) in skipped {
            tracing::warn!(
                "MCP server `{name}` in scope `{}` ignored: its tool names would collide \
                 with `{other}`'s — rename one",
                scope.key
            );
        }
        let usable: Vec<String> = usable.into_iter().map(str::to_string).collect();
        for name in &usable {
            let this = self.clone();
            let scope = scope.clone();
            let name = name.clone();
            pending.spawn(async move {
                let result = this.ensure_server(&scope, &name).await;
                (name, result)
            });
        }
        let mut failures = Vec::new();
        while let Some(joined) = pending.join_next().await {
            match joined {
                Ok((_, Ok(_))) => {}
                Ok((name, Err(e))) => failures.push((name, e)),
                Err(e) => failures.push(("<join>".to_string(), e.into())),
            }
        }
        failures
    }

    /// The dispatch overlay for a call to `tool`: for a scoped session,
    /// replace the snapshot's `mcp__*` entries with the scope's own tools —
    /// lazily connecting the one server `tool` belongs to first. `Err` is a
    /// user-facing tool-error string (auth-required, connect failure) the
    /// caller replies with; the model sees a normal failed tool call, which is
    /// what a multi-user embedder keys its authorization prompt off.
    pub async fn overlay_registry_for_call(
        &self,
        session: &SessionId,
        base: ToolRegistry,
        tool: &str,
    ) -> Result<ToolRegistry, String> {
        let Some(scope) = self.scope_of(session) else {
            return Ok(base);
        };
        if let Some(name) = owning_server(&scope, tool) {
            self.ensure_server(&scope, &name)
                .await
                .map_err(|e| render_scope_error(&name, &e))?;
        }
        Ok(self.replace_mcp_tools(&scope, base))
    }

    /// Drop a scope's cached connections (logout, a changed server set —
    /// config drift under an unchanged key is invisible to the cache, so a
    /// changed URL or store *requires* this). The next call reconnects fresh;
    /// dropping the last `Arc<McpClient>` reaps a stdio server's child.
    pub fn evict_scope(&self, key: &str) {
        let mut cache = self.cache.lock().expect("scope cache lock poisoned");
        // Bumped under the cache lock: a connect that started before this
        // eviction (a detached script connect, #831) cannot land after it.
        *self
            .generations
            .lock()
            .expect("scope generation lock poisoned")
            .entry(key.to_string())
            .or_default() += 1;
        cache.retain(|(k, _), _| k != key);
        drop(cache);
        self.connecting
            .lock()
            .expect("scope connect-guard lock poisoned")
            .retain(|(k, _), _| k != key);
    }

    /// Swap the snapshot's global `mcp__*` tools for the scope's cached ones.
    fn replace_mcp_tools(&self, scope: &McpScope, mut base: ToolRegistry) -> ToolRegistry {
        base.unregister_prefix("mcp__");
        let cache = self.cache.lock().expect("scope cache lock poisoned");
        for name in scope.servers.keys() {
            if let Some(server) = cache.get(&(scope.key.clone(), name.clone())) {
                for tool in &server.tools {
                    base.register_arc(tool.clone());
                }
            }
        }
        base
    }
}

/// Which of the scope's servers a namespaced tool name belongs to, matching by
/// the same sanitized prefix [`McpTool::new`](super::McpTool::new) advertises under — so a server
/// name that was sanitized (`my server` → `my_server`) still matches its own
/// tools.
/// A disabled or clashed-out scoped server owns nothing: unreachable, like
/// `list_mcp` never lists it.
fn owning_server(scope: &McpScope, tool: &str) -> Option<String> {
    super::tool::owning_server(usable_servers(scope).0, tool).map(str::to_string)
}

/// The scope's servers that can register tools — not disabled and, by name
/// order, not clashing with an earlier one ([`super::tool::names_clash`], the
/// global roster's rule) — plus each skipped `(server, clashes with)`.
fn usable_servers(scope: &McpScope) -> (Vec<&str>, Vec<(&str, &str)>) {
    let mut names: Vec<&str> = scope
        .servers
        .iter()
        .filter(|(_, cfg)| !cfg.disabled)
        .map(|(name, _)| name.as_str())
        .collect();
    names.sort_unstable();
    let (mut kept, mut skipped): (Vec<&str>, Vec<(&str, &str)>) = (Vec::new(), Vec::new());
    for name in names {
        match kept.iter().find(|k| super::tool::names_clash(k, name)) {
            Some(other) => skipped.push((name, other)),
            None => kept.push(name),
        }
    }
    (kept, skipped)
}

fn auth_required_message(name: &str) -> String {
    format!(
        "MCP server `{name}` requires authorization for this user; \
         complete the authorization flow and retry"
    )
}

/// Flatten an [`ensure_server`][McpScopes::ensure_server] failure into the
/// tool-error string the model sees, keeping the auth-required case crisp —
/// both the precheck refusal and a live 401 (`is_auth_required`) render the
/// same way, so the embedder has one message shape to key off.
fn render_scope_error(name: &str, e: &anyhow::Error) -> String {
    if entanglement_core::is_auth_required(e) {
        return auth_required_message(name);
    }
    let msg = format!("{e:#}");
    if msg.contains("requires authorization for this user") {
        msg
    } else {
        format!("MCP server `{name}` connect failed: {msg}")
    }
}

// The connect side — the guarded lazy connect, eviction generations, the
// script path's connect target — split out along the 400-line file cap.
#[path = "scoped_connect.rs"]
mod connect;
pub(crate) use connect::ScopedConnect;

// `list_mcp`'s scoped rows and the scoped dispatch decline, split out along
// the 400-line file cap (a `#[path]` child, so private fields stay reachable).
#[path = "scoped_list.rs"]
mod list;

#[cfg(test)]
#[path = "scoped_tests.rs"]
mod tests;
