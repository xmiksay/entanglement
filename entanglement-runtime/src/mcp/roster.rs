//! One row per registered MCP server (ADR-0226: registered = available) —
//! the shared view behind the `McpList` wire snapshot (`/mcp`) and the
//! model-facing `list_mcp`. Pure reads of the roster and the connected set:
//! listing never connects anything.

use super::live::ActiveServers;
use super::{transport_label, AvailableMcp};

/// A registered server as the heads and the model see it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerRow {
    pub name: String,
    pub description: Option<String>,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    /// What the transport points at (the last-seen cache key).
    pub target: String,
    pub connected: bool,
    /// Registered (`mcp__<server>__<tool>`) names — empty unless connected.
    pub tools: Vec<String>,
    /// Declares `oauth:`; `auth_required` when no credential is stored yet.
    pub oauth: bool,
    pub auth_required: bool,
}

/// Every available server plus anything connected outside the roster (a
/// server live-added by another path), name-sorted. Keyless bundles and
/// `disabled` servers are absent.
pub fn roster(avail: &AvailableMcp, active: &ActiveServers) -> Vec<ServerRow> {
    // Snapshot and release: the rows below read the OAuth token file, which
    // must not happen under a lock every dispatch also takes.
    let active: std::collections::HashMap<String, (String, Vec<String>)> = active
        .lock()
        .expect("MCP active-server mutex poisoned")
        .iter()
        .map(|(name, s)| (name.clone(), (s.transport.clone(), s.tools.clone())))
        .collect();
    let mut rows: Vec<ServerRow> = avail
        .available_names()
        .into_iter()
        .filter_map(|name| {
            let server = avail.get(&name)?;
            let cfg = &server.config;
            let connected = active.get(&name);
            Some(ServerRow {
                description: cfg.description.clone(),
                transport: transport_label(cfg),
                target: cfg.target(),
                connected: connected.is_some(),
                tools: connected
                    .map(|(_, tools)| tools.clone())
                    .unwrap_or_default(),
                oauth: cfg.oauth.is_some(),
                auth_required: connected.is_none() && avail.needs_auth(&name, cfg),
                name,
            })
        })
        .collect();
    for (name, (transport, tools)) in active {
        if rows.iter().any(|r| r.name == name) {
            continue;
        }
        rows.push(ServerRow {
            name,
            description: None,
            transport,
            target: String::new(),
            connected: true,
            tools,
            oauth: false,
            auth_required: false,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::mcp::McpServerConfig;

    fn cfg(url: &str, oauth: bool) -> McpServerConfig {
        McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.into()),
            headers: HashMap::new(),
            disabled: false,
            capabilities: HashMap::new(),
            oauth: oauth.then(Default::default),
            description: Some(format!("about {url}")),
        }
    }

    #[test]
    fn an_unconnected_server_is_listed_with_its_description_and_auth_posture() {
        let avail =
            AvailableMcp::default().with_token_store(crate::config::McpTokenStore::at(None));
        avail.insert_user("kb", cfg("https://kb/mcp", false));
        avail.insert_user("remote", cfg("https://remote-0226-unauthed/mcp", true));
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let rows = roster(&avail, &active);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "kb");
        assert!(!rows[0].connected && rows[0].tools.is_empty());
        assert_eq!(rows[0].description.as_deref(), Some("about https://kb/mcp"));
        assert_eq!(rows[0].target, "https://kb/mcp");
        assert!(!rows[0].oauth && !rows[0].auth_required);
        assert!(rows[1].oauth && rows[1].auth_required);
    }
}
