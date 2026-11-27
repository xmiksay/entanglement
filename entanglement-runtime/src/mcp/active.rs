//! What is connected right now (#375): [`ActiveServers`] is empty at startup
//! (ADR-0226), filled by lazy connects and an embedder's live add, drained by
//! its live remove — each through [`install_server`] / [`drop_server`], which
//! swap exactly the tools a connection registered.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::tools::SharedRegistry;

use super::{register_tools, transport_label, McpClient, McpServerConfig};

/// One connected MCP server: its live client handle (dropping the last `Arc`
/// kills the subprocess / closes the HTTP session — `StdioClient`'s
/// `kill_on_drop`) and the tool names it registered.
pub struct ActiveServer {
    pub client: Arc<McpClient>,
    pub tools: Vec<String>,
    /// `"stdio"` or `"http"`.
    pub transport: String,
}

/// Shared, mutated by the MCP responder task (#375) on every add/remove.
pub type ActiveServers = Arc<Mutex<HashMap<String, ActiveServer>>>;

/// Replace `name`'s connection: unregister exactly the tools its previous
/// connection registered (never a name prefix — `my.docs` registers as
/// `my_docs`, and server `a`'s prefix also covers `a__b`'s tools), register
/// `defs`, and track the new client. Returns the registered names.
pub fn install_server(
    registry: &SharedRegistry,
    active: &ActiveServers,
    name: &str,
    client: Arc<McpClient>,
    defs: Vec<super::McpToolDef>,
    cfg: &McpServerConfig,
) -> Vec<String> {
    let previous = active
        .lock()
        .expect("MCP active-server mutex poisoned")
        .remove(name);
    let tools = {
        let mut reg = registry.write().expect("tool registry lock poisoned");
        for tool in previous.iter().flat_map(|p| &p.tools) {
            reg.unregister(tool);
        }
        register_tools(&mut reg, &client, name, defs, &cfg.capabilities)
    };
    active
        .lock()
        .expect("MCP active-server mutex poisoned")
        .insert(
            name.to_string(),
            ActiveServer {
                client,
                tools: tools.clone(),
                transport: transport_label(cfg),
            },
        );
    tools
}

/// Drop `name`'s connection and exactly the tools it registered (see
/// [`install_server`] for why never by prefix). A name never connected is a
/// no-op.
pub fn drop_server(registry: &SharedRegistry, active: &ActiveServers, name: &str) {
    let previous = active
        .lock()
        .expect("MCP active-server mutex poisoned")
        .remove(name);
    let mut reg = registry.write().expect("tool registry lock poisoned");
    for tool in previous.iter().flat_map(|p| &p.tools) {
        reg.unregister(tool);
    }
}

/// Enumerate every currently-connected server, sorted by name for stable
/// output.
pub fn mcp_list(active: &ActiveServers) -> Vec<entanglement_core::McpServerStatus> {
    let active = active.lock().expect("MCP active-server mutex poisoned");
    let mut list: Vec<entanglement_core::McpServerStatus> = active
        .iter()
        .map(|(name, server)| entanglement_core::McpServerStatus {
            name: name.clone(),
            transport: server.transport.clone(),
            connected: true,
            tools: server.tools.clone(),
            error: None,
            // Filled by the head's responder, which holds the token store.
            auth: None,
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    use crate::tools::ToolRegistry;

    fn shared_registry() -> SharedRegistry {
        Arc::new(RwLock::new(ToolRegistry::new()))
    }

    fn stdio_cfg(command: &str) -> McpServerConfig {
        McpServerConfig {
            command: Some(command.to_string()),
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            disabled: false,
            capabilities: HashMap::new(),
            oauth: None,
            description: None,
        }
    }

    #[test]
    fn mcp_list_is_empty_with_no_active_servers() {
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        assert!(mcp_list(&active).is_empty());
    }

    // `fake_client` spawns the reader task via `tokio::spawn`, which needs a
    // live runtime — `#[tokio::test]`, not a plain `#[test]`.
    #[tokio::test]
    async fn mcp_list_sorts_by_name() {
        let mut map = HashMap::new();
        map.insert(
            "zeta".to_string(),
            ActiveServer {
                client: Arc::new(fake_client()),
                tools: vec!["mcp__zeta__a".into()],
                transport: "stdio".into(),
            },
        );
        map.insert(
            "alpha".to_string(),
            ActiveServer {
                client: Arc::new(fake_client()),
                tools: vec!["mcp__alpha__a".into()],
                transport: "http".into(),
            },
        );
        let active: ActiveServers = Arc::new(Mutex::new(map));
        let list = mcp_list(&active);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[1].name, "zeta");
        assert!(list[0].connected);
        assert_eq!(list[0].tools, vec!["mcp__alpha__a".to_string()]);
    }

    fn defs(names: &[&str]) -> Vec<crate::mcp::McpToolDef> {
        names
            .iter()
            .map(|n| crate::mcp::McpToolDef {
                name: n.to_string(),
                description: String::new(),
                input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            })
            .collect()
    }

    /// A reconnect of a server whose name sanitizes (`my.docs` → `my_docs`)
    /// drops every tool the old connection registered — none goes stale.
    #[tokio::test]
    async fn a_reconnect_replaces_a_sanitized_servers_tools_exactly() {
        let registry = shared_registry();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let cfg = stdio_cfg("x");
        let client = Arc::new(fake_client());
        install_server(
            &registry,
            &active,
            "my.docs",
            client.clone(),
            defs(&["old"]),
            &cfg,
        );
        assert!(registry.read().unwrap().contains("mcp__my_docs__old"));
        install_server(&registry, &active, "my.docs", client, defs(&["new"]), &cfg);
        let reg = registry.read().unwrap();
        assert!(!reg.contains("mcp__my_docs__old"), "stale tool left behind");
        assert!(reg.contains("mcp__my_docs__new"));
    }

    /// Dropping server `a` never touches `a__b`'s tools, though `mcp__a__` is
    /// a prefix of them.
    #[tokio::test]
    async fn dropping_a_server_leaves_a_nested_named_servers_tools() {
        let registry = shared_registry();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let cfg = stdio_cfg("x");
        install_server(
            &registry,
            &active,
            "a",
            Arc::new(fake_client()),
            defs(&["x"]),
            &cfg,
        );
        install_server(
            &registry,
            &active,
            "a__b",
            Arc::new(fake_client()),
            defs(&["y"]),
            &cfg,
        );
        drop_server(&registry, &active, "a");
        let reg = registry.read().unwrap();
        assert!(!reg.contains("mcp__a__x"));
        assert!(reg.contains("mcp__a__b__y"), "a__b's tool must survive");
        assert!(active.lock().unwrap().contains_key("a__b"));
    }

    // A minimal `McpClient` to populate `ActiveServer.client` in tests that
    // never actually call it: an in-memory duplex pipe stands in for a
    // subprocess, and `child: None` means drop is a no-op, not a real kill.
    fn fake_client() -> McpClient {
        let (a, _b) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(a);
        McpClient::Stdio(crate::mcp::StdioClient::new(
            "fake".to_string(),
            writer,
            reader,
            None,
        ))
    }
}
