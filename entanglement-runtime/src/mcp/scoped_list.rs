//! The per-user scope's discovery side (ADR-0188 + ADR-0226): `list_mcp`'s
//! rows for a scoped session, and the decline for an `mcp__*` call the scope
//! cannot serve. Split out of `scoped.rs` along the 400-line file cap.

use entanglement_core::SessionId;

use super::super::{transport_label, ServerRow};
use super::{owning_server, McpScopes};
use crate::tools::ToolRegistry;

impl McpScopes {
    /// `list_mcp`'s rows for a scoped session — the scope's servers only, a
    /// global server never shows — or `None` for an unscoped one. Tool names
    /// come from this scope's live connections only: the shared last-seen
    /// cache is never consulted, since one target may expose different tools
    /// per credential.
    pub fn rows(&self, session: &SessionId) -> Option<Vec<ServerRow>> {
        let scope = self.scope_of(session)?;
        // Snapshot this scope's connected tool names and release the cache
        // lock: the rows below read the token store, which must not happen
        // under a lock every scoped dispatch also takes.
        let connected: std::collections::HashMap<String, Vec<String>> = {
            let cache = self.cache.lock().expect("scope cache lock poisoned");
            scope
                .servers
                .keys()
                .filter_map(|name| {
                    let server = cache.get(&(scope.key.clone(), name.clone()))?;
                    let tools = server.tools.iter().map(|t| t.name().into_owned());
                    Some((name.clone(), tools.collect()))
                })
                .collect()
        };
        let usable = super::usable_servers(&scope).0;
        let mut rows: Vec<ServerRow> = scope
            .servers
            .iter()
            .filter(|(name, _)| usable.contains(&name.as_str()))
            .map(|(name, cfg)| {
                let connected = connected.get(name);
                let has_token = || {
                    scope
                        .token_store
                        .as_ref()
                        .and_then(|store| store.load(name).ok().flatten())
                        .is_some()
                };
                ServerRow {
                    name: name.clone(),
                    description: cfg.description.clone(),
                    transport: transport_label(cfg),
                    target: cfg.target(),
                    connected: connected.is_some(),
                    tools: connected.cloned().unwrap_or_default(),
                    oauth: cfg.oauth.is_some(),
                    auth_required: connected.is_none() && cfg.oauth.is_some() && !has_token(),
                }
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Some(rows)
    }

    /// For a scoped session, the decline for an `mcp__*` call its scope cannot
    /// serve, given the call's already-overlaid `registry` — the call must
    /// never fall through to the global roster (ADR-0188). `None` when the
    /// session is unscoped or the scope serves the tool.
    pub fn scope_decline(
        &self,
        session: &SessionId,
        registry: &ToolRegistry,
        tool: &str,
    ) -> Option<String> {
        if !tool.starts_with("mcp__") || registry.contains(tool) {
            return None;
        }
        let scope = self.scope_of(session)?;
        Some(match owning_server(&scope, tool) {
            Some(server) => crate::mcp::available::missing_tool_decline(&server, tool),
            None => crate::mcp::available::unknown_server_decline(
                crate::mcp::available::server_name_of(tool).unwrap_or(tool),
            ),
        })
    }
}
