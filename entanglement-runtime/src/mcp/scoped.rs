//! Session-keyed per-user MCP scopes (#684) — the runtime consumption of
//! ADR-0184's per-user credential seam.
//!
//! A multi-user embedder gives each user their own MCP server set and OAuth
//! tokens without the provider crate's user-id type ever entering this crate
//! (ADR-0181, the grep gate):
//! it supplies an [`McpScopeResolver`] — a closure from `SessionId` to that
//! session's [`McpScope`] — and the runtime consults it at advertisement and
//! dispatch time. The session→user mapping stays the embedder's private
//! concern; the scope's `key` is an opaque string it derives from its own user
//! identity, and the credential slice is typically
//! `entanglement_provider::user_scoped(store, user)`.
//!
//! **Replace semantics**: a scoped session's `mcp__*` namespace is entirely
//! scope-owned — the global registry's MCP tools are stripped from its specs
//! and dispatch snapshots, and the scope's own tools take their place. That is
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
//! process-lifetime precedent. The resolver runs on the advertisement path
//! (core's sync `ToolSpecResolver`) and per MCP dispatch, so it must be cheap
//! and non-blocking — an in-memory map lookup, never I/O.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use entanglement_core::{SessionId, ToolSpec};

use crate::tools::{Tool, ToolRegistry};

use super::tool::namespaced_tool_name;
use super::{McpClient, McpServerConfig, McpTool};

/// Ceiling on a lazy per-scope connect, mirroring `available_enable.rs` (#556):
/// a hung server must not park the calling turn forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

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
/// unscoped and sees the global MCP behavior unchanged. Runs on the sync
/// advertisement path — keep it an in-memory lookup.
pub type McpScopeResolver = Arc<dyn Fn(&SessionId) -> Option<McpScope> + Send + Sync>;

/// A connected per-scope server: the shared client, its registered-shape tool
/// proxies, and the cached `tools/list` specs the sync advertisement path
/// serves.
struct ScopeServer {
    tools: Vec<Arc<dyn Tool>>,
    specs: Vec<ToolSpec>,
    /// Keeps the connection (and a stdio server's child) alive as long as the
    /// cache entry does, exactly like `ActiveServer` — the tool proxies above
    /// each hold their own `Arc<McpClient>` clone already, this field just
    /// makes the ownership explicit.
    _client: Arc<McpClient>,
}

/// The per-scope MCP connection cache + overlay entry points. Construct one
/// per embedded engine and hand it to
/// [`spawn_tool_executor_with_policy`][crate::tool_runner::spawn_tool_executor_with_policy];
/// wrap your `tool_spec_resolver` with [`overlay_specs`][Self::overlay_specs].
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
        })
    }

    /// The session's scope, per the embedder's resolver.
    pub fn scope_of(&self, session: &SessionId) -> Option<McpScope> {
        (self.resolver)(session)
    }

    /// Connect every server in the session's scope, concurrently and
    /// best-effort, so the scope's tools are listed (and advertisable) before
    /// the first prompt. Returns per-server failures — empty means every
    /// server connected. Embedders call this between `Spawn` and the first
    /// `Prompt`; a scoped session that was never prewarmed advertises no MCP
    /// tools until a dispatch lazily connects one.
    pub async fn prewarm(self: &Arc<Self>, session: &SessionId) -> Vec<(String, anyhow::Error)> {
        let Some(scope) = self.scope_of(session) else {
            return Vec::new();
        };
        let scope = Arc::new(scope);
        let mut pending = tokio::task::JoinSet::new();
        for name in scope.servers.keys() {
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

    /// The advertisement overlay: for a scoped session, strip every global
    /// `mcp__*` spec and advertise the scope's *cached* (already-connected)
    /// tools instead, re-sorted by name (#566). Unscoped sessions pass
    /// through untouched. Sync on purpose — it runs inside core's
    /// [`ToolSpecResolver`][entanglement_core::ToolSpecResolver] closure, so
    /// listings come from the cache ([`prewarm`][Self::prewarm]), never from
    /// I/O here.
    pub fn overlay_specs(&self, session: &SessionId, specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
        let Some(scope) = self.scope_of(session) else {
            return specs;
        };
        let mut specs: Vec<ToolSpec> = specs
            .into_iter()
            .filter(|s| !s.name.starts_with("mcp__"))
            .collect();
        let cache = self.cache.lock().expect("scope cache lock poisoned");
        for name in scope.servers.keys() {
            if let Some(server) = cache.get(&(scope.key.clone(), name.clone())) {
                specs.extend(server.specs.iter().cloned());
            }
        }
        drop(cache);
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
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

    /// The cached-only dispatch overlay (the `rhai` arm): scope tools already
    /// connected are visible to a script's bindings, but a script call never
    /// triggers a lazy connect — scripts run against a pre-spawn snapshot, and
    /// blocking the executor loop on a connect is not acceptable there.
    pub fn overlay_registry_cached(&self, session: &SessionId, base: ToolRegistry) -> ToolRegistry {
        match self.scope_of(session) {
            Some(scope) => self.replace_mcp_tools(&scope, base),
            None => base,
        }
    }

    /// Drop a scope's cached connections (logout, a changed server set —
    /// config drift under an unchanged key is invisible to the cache, so a
    /// changed URL or store *requires* this). The next call reconnects fresh;
    /// dropping the last `Arc<McpClient>` reaps a stdio server's child.
    pub fn evict_scope(&self, key: &str) {
        self.cache
            .lock()
            .expect("scope cache lock poisoned")
            .retain(|(k, _), _| k != key);
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

    /// Connect `name` for `scope` if it isn't cached yet — the
    /// `available_enable.rs` double-checked critical section, keyed by
    /// `(scope key, server)` instead of server alone.
    async fn ensure_server(&self, scope: &McpScope, name: &str) -> Result<Arc<ScopeServer>> {
        let cache_key = (scope.key.clone(), name.to_string());
        if let Some(server) = self
            .cache
            .lock()
            .expect("scope cache lock poisoned")
            .get(&cache_key)
        {
            return Ok(server.clone());
        }
        let guard = {
            let mut connecting = self
                .connecting
                .lock()
                .expect("scope connect-guard lock poisoned");
            connecting.entry(cache_key.clone()).or_default().clone()
        };
        let _permit = guard.lock().await;
        // Re-check under the guard — a concurrent first call may have won.
        let already = self
            .cache
            .lock()
            .expect("scope cache lock poisoned")
            .get(&cache_key)
            .cloned();
        if let Some(server) = already {
            return Ok(server);
        }
        let Some(cfg) = scope.servers.get(name) else {
            bail!("scope has no MCP server named `{name}`");
        };
        // An OAuth server with no stored credential for this scope fails
        // *before* the connect — attempting it would only produce a 401
        // (mirroring the global startup skip), and the clean auth-required
        // error is what the embedder keys its authorization flow off.
        if cfg.oauth.is_some() {
            let stored = match &scope.token_store {
                None => None,
                Some(store) => store
                    .load(name)
                    .with_context(|| format!("loading the stored credential for `{name}`"))?,
            };
            if stored.is_none() {
                bail!(auth_required_message(name));
            }
        }
        let (client, defs) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            super::connect_impl::connect_client_with_store(
                name,
                cfg,
                &self.secret_env,
                &self.http,
                None,
                None,
                scope.token_store.clone(),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("connecting MCP server `{name}` timed out"))??;
        let tools: Vec<Arc<dyn Tool>> = defs
            .into_iter()
            .map(|def| Arc::new(McpTool::new(client.clone(), name, def)) as Arc<dyn Tool>)
            .collect();
        let specs: Vec<ToolSpec> = tools
            .iter()
            .map(|t| ToolSpec::with_schema(t.name(), t.description(), t.schema()))
            .collect();
        let server = Arc::new(ScopeServer {
            tools,
            specs,
            _client: client,
        });
        self.cache
            .lock()
            .expect("scope cache lock poisoned")
            .insert(cache_key, server.clone());
        tracing::info!(
            "MCP server `{name}`: connected for scope `{}`, {} tool(s)",
            scope.key,
            server.specs.len()
        );
        Ok(server)
    }
}

/// Which of the scope's servers a namespaced tool name belongs to, matching by
/// the same sanitized prefix [`McpTool::new`] advertises under — so a server
/// name that was sanitized (`my server` → `my_server`) still matches its own
/// tools.
fn owning_server(scope: &McpScope, tool: &str) -> Option<String> {
    scope
        .servers
        .keys()
        .find(|name| tool.starts_with(&namespaced_tool_name(name, "")))
        .cloned()
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

#[cfg(test)]
#[path = "scoped_tests.rs"]
mod tests;
