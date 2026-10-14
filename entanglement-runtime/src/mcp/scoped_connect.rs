//! The per-user scope's connect side (ADR-0188): the guarded lazy connect,
//! the eviction generation that keeps a connect begun before
//! [`McpScopes::evict_scope`] from caching after it, and the script path's
//! detached connect (#831). Split out of `scoped.rs` along the 400-line cap.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use entanglement_core::SessionId;

use super::super::McpTool;
use super::{
    auth_required_message, owning_server, render_scope_error, usable_servers, McpScope, McpScopes,
    ScopeServer,
};
use crate::tools::Tool;

/// Ceiling on a lazy per-scope connect, mirroring `available_enable.rs` (#556):
/// a hung server must not park the calling turn forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// What a session's call to an `mcp__*` tool needs from its scope first.
pub(crate) enum ScopedConnect {
    /// The session is unscoped: the global path applies.
    Unscoped,
    /// Nothing to connect — no scope server owns the tool, or it is cached.
    Ready,
    /// `server` must connect for `scope` first.
    Needed { scope: McpScope, server: String },
}

impl McpScopes {
    /// Classify `session`'s call to `tool` without connecting anything.
    pub(crate) fn connect_needed(&self, session: &SessionId, tool: &str) -> ScopedConnect {
        let Some(scope) = self.scope_of(session) else {
            return ScopedConnect::Unscoped;
        };
        let Some(server) = owning_server(&scope, tool) else {
            return ScopedConnect::Ready;
        };
        let cached = self
            .cache
            .lock()
            .expect("scope cache lock poisoned")
            .contains_key(&(scope.key.clone(), server.clone()));
        if cached {
            ScopedConnect::Ready
        } else {
            ScopedConnect::Needed { scope, server }
        }
    }

    /// Join (or start) the one detached connect of `server` for `scope`;
    /// `Err` is the tool-error text.
    pub(crate) fn script_connect(
        self: &Arc<Self>,
        scope: McpScope,
        server: String,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        let this = self.clone();
        // The generation keeps a call after `evict_scope` off the evicted
        // connect, which could only end in "was evicted while connecting".
        let key = format!("{}\0{}\0{server}", scope.key, self.generation(&scope.key));
        self.script_connects.join(key, move || async move {
            this.ensure_server(&scope, &server)
                .await
                .map(drop)
                .map_err(|e| render_scope_error(&server, &e))
        })
    }

    /// Connect `name` for `scope` if it isn't cached yet — the
    /// `available_enable.rs` double-checked critical section, keyed by
    /// `(scope key, server)` instead of server alone.
    pub(super) async fn ensure_server(
        &self,
        scope: &McpScope,
        name: &str,
    ) -> Result<Arc<ScopeServer>> {
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
        let generation = self.generation(&scope.key);
        let usable = usable_servers(scope).0.contains(&name);
        let Some(cfg) = scope.servers.get(name).filter(|_| usable) else {
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
            super::super::connect_impl::connect_client_with_store(
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
            .map(|def| {
                Arc::new(McpTool::new(client.clone(), name, def, &cfg.capabilities))
                    as Arc<dyn Tool>
            })
            .collect();
        let server = Arc::new(ScopeServer {
            tools,
            _client: client,
        });
        if !self.cache_if_current(cache_key, generation, server.clone()) {
            bail!(
                "MCP scope `{}` was evicted while `{name}` was connecting",
                scope.key
            );
        }
        tracing::info!(
            "MCP server `{name}`: connected for scope `{}`, {} tool(s)",
            scope.key,
            server.tools.len()
        );
        Ok(server)
    }

    fn generation(&self, key: &str) -> u64 {
        self.generations
            .lock()
            .expect("scope generation lock poisoned")
            .get(key)
            .copied()
            .unwrap_or_default()
    }

    /// Cache `server` unless its scope was evicted since `generation` was
    /// read; same lock order as `evict_scope` (cache, then generations).
    fn cache_if_current(
        &self,
        cache_key: (String, String),
        generation: u64,
        server: Arc<ScopeServer>,
    ) -> bool {
        let mut cache = self.cache.lock().expect("scope cache lock poisoned");
        if self.generation(&cache_key.0) != generation {
            return false;
        }
        cache.insert(cache_key, server);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead_server() -> Arc<ScopeServer> {
        Arc::new(ScopeServer {
            tools: Vec::new(),
            _client: super::super::super::tool::dead_client(),
        })
    }

    /// #831: a connect that began before `evict_scope` never re-inserts the
    /// evicted user's connection.
    #[tokio::test]
    async fn a_connect_begun_before_an_eviction_is_not_cached() {
        let scopes = McpScopes::new(Arc::new(|_| None), crate::test_http_client(), Vec::new());
        let key = ("user-a".to_string(), "kb".to_string());
        let before = scopes.generation("user-a");
        scopes.evict_scope("user-a");
        assert!(!scopes.cache_if_current(key.clone(), before, dead_server()));
        assert!(scopes.cache.lock().unwrap().is_empty());
        let now = scopes.generation("user-a");
        assert!(scopes.cache_if_current(key, now, dead_server()));
        // Another scope's eviction leaves this one's generation alone.
        scopes.evict_scope("user-b");
        assert_eq!(scopes.generation("user-a"), now);
    }

    /// #831: a script call after `evict_scope` gets its own connect, never
    /// the evicted one's outcome.
    #[tokio::test]
    async fn a_script_connect_after_an_eviction_starts_its_own() {
        let dir = std::env::temp_dir().join(format!("scope-evict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spawns = dir.join("spawns");
        let mut cfg = crate::mcp::available::tests::stdio("sh");
        cfg.args = vec![
            "-c".into(),
            format!("echo x >> '{}'; sleep 0.3", spawns.display()),
        ];
        let servers = std::collections::HashMap::from([("kb".to_string(), cfg)]);
        let scopes = McpScopes::new(
            Arc::new(move |_| {
                Some(McpScope {
                    key: "user-a".into(),
                    servers: servers.clone(),
                    token_store: None,
                })
            }),
            crate::test_http_client(),
            Vec::new(),
        );
        let session = SessionId::new("s");
        let scope = || scopes.scope_of(&session).expect("scoped");
        let first = scopes.script_connect(scope(), "kb".into());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        scopes.evict_scope("user-a");
        let second = scopes.script_connect(scope(), "kb".into());
        let (first, second) = (first.await, second.await);
        let count = std::fs::read_to_string(&spawns).unwrap().lines().count();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(count, 2, "the post-eviction call connects on its own");
        for err in [first, second] {
            let err = err.expect_err("the stub never completes a handshake");
            assert!(!err.contains("evicted"), "{err}");
        }
    }
}
