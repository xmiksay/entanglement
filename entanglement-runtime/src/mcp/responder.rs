//! Runtime service answering the global MCP wire ops (#375) —
//! `InMsg::McpList`/`McpAdd`/`McpRemove` — off the inbound fan-out.
//!
//! MCP config is engine-global, not one session's, so core routes none of
//! these to a session task (`msg_to_cmd` → `None`, mirroring
//! `InMsg::ListSessions`). This mirrors
//! [`crate::history::spawn_history_responder`]'s answer to `ReplayFrom`: a
//! runtime-side subscriber is the sole answerer, since it alone holds the
//! `SharedRegistry` + `ActiveServers` + live server-config map these ops read
//! and mutate.
//!
//! A failed `McpAdd`/`McpRemove` is logged, not surfaced as an `OutEvent` —
//! there is no session to attach an error to, and this matches the existing
//! MCP philosophy throughout this module: a server attach is best-effort,
//! failures are diagnostic, never fatal to the caller's turn.

use std::sync::Arc;

use entanglement_core::{Holly, InMsg, McpAction, McpServerStatus};
use tokio::sync::broadcast::error::RecvError;

use crate::tools::SharedRegistry;

use super::available::AvailableMcp;
use super::live::{mcp_add, mcp_list, mcp_remove, ActiveServers, ServerConfigs};
use super::transport_label;

/// Spawns a subscriber that answers `InMsg::McpList`/`McpAdd`/`McpRemove`.
/// `secret_env` (the catalog's provider API-key env vars, #164) is scrubbed
/// from any stdio server a live add spawns. `avail` (#542) contributes the
/// available-but-unconnected `allowed` servers to `McpList` (keyless bundles
/// stay silently absent) and routes a `McpRemove` of a lazily-connected
/// bundled server to a plain disconnect instead of a config-map removal.
pub fn spawn_mcp_responder(
    holly: &Holly,
    registry: SharedRegistry,
    active: ActiveServers,
    configs: ServerConfigs,
    avail: Arc<AvailableMcp>,
    secret_env: Vec<String>,
) -> tokio::task::JoinHandle<()> {
    let emitter = holly.clone();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        loop {
            match inbound.recv().await {
                Ok(InMsg::McpList { correlation_id }) => {
                    let servers = full_list(&active, &avail);
                    emitter.emit_mcp_list(correlation_id, servers);
                }
                Ok(InMsg::McpAdd { name, config }) => {
                    match mcp_add(
                        name.clone(),
                        config.into(),
                        &registry,
                        &active,
                        &configs,
                        &secret_env,
                    )
                    .await
                    {
                        Ok(tools) => {
                            tracing::info!(server = %name, tools = tools.len(), "MCP: live-added");
                            emitter.emit_mcp_changed(name, McpAction::Added);
                        }
                        Err(e) => tracing::warn!(server = %name, "MCP add failed: {e:#}"),
                    }
                }
                Ok(InMsg::McpRemove { name }) => {
                    // A bundled/`allowed` server (#542) has no config-map entry
                    // to remove — disconnect it (it stays *available*) instead
                    // of erroring halfway through `mcp_remove`.
                    if avail.is_lazy(&name) || avail.get(&name).is_some() {
                        super::available::disconnect(&avail, &name, &registry, &active);
                        emitter.emit_mcp_changed(name, McpAction::Removed);
                    } else {
                        match mcp_remove(&name, &registry, &active, &configs) {
                            Ok(()) => emitter.emit_mcp_changed(name, McpAction::Removed),
                            Err(e) => tracing::warn!(server = %name, "MCP remove failed: {e:#}"),
                        }
                    }
                }
                Ok(_) => {}
                // A dropped inbound frame under lag can only lose a query/command —
                // the head times out and re-asks; keep serving.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("MCP responder lagged, skipped {n} inbound messages");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// The `McpList` snapshot (#542): every connected server (state `enabled`,
/// or `allowed` when it was lazily connected) plus every available-but-
/// unconnected `allowed` server — keyless bundles silently absent. Sorted by
/// name for stable output.
fn full_list(active: &ActiveServers, avail: &AvailableMcp) -> Vec<McpServerStatus> {
    let mut list = mcp_list(active);
    for s in &mut list {
        let state = if avail.is_lazy(&s.name) {
            "allowed"
        } else {
            "enabled"
        };
        s.state = Some(state.to_string());
    }
    for name in avail.available_names() {
        if list.iter().any(|s| s.name == name) {
            continue;
        }
        let transport = avail
            .get(&name)
            .map(|s| transport_label(&s.config))
            .unwrap_or_default();
        list.push(McpServerStatus {
            name,
            transport,
            connected: false,
            tools: Vec::new(),
            error: None,
            state: Some("allowed".to_string()),
        });
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, RwLock};

    use entanglement_core::{EngineConfig, McpServerSpec, OutEvent};

    use super::*;
    use crate::tools::ToolRegistry;

    fn empty_engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    /// #542: an available (`allowed`, unconnected) server rides the `McpList`
    /// snapshot with `connected: false` and `state: "allowed"`.
    #[test]
    fn full_list_includes_available_servers() {
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let mut user = HashMap::new();
        user.insert(
            "mine".to_string(),
            crate::mcp::McpServerConfig {
                command: None,
                args: vec![],
                env: HashMap::new(),
                url: Some("https://example.com/mcp".into()),
                headers: HashMap::new(),
                disabled: false,
                capabilities: HashMap::new(),
                state: Some(crate::mcp::McpServerState::Allowed),
            },
        );
        let (_, avail) = AvailableMcp::partition(
            &entanglement_core::Catalog { providers: vec![] },
            &user,
            vec![],
        );
        let list = full_list(&active, &avail);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "mine");
        assert!(!list[0].connected);
        assert_eq!(list[0].state.as_deref(), Some("allowed"));
        assert_eq!(list[0].transport, "http");
        assert!(list[0].tools.is_empty());
    }

    #[tokio::test]
    async fn mcp_list_replies_with_an_empty_snapshot() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let registry: SharedRegistry = Arc::new(RwLock::new(ToolRegistry::new()));
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));
        let handle = spawn_mcp_responder(
            &holly,
            registry,
            active,
            configs,
            Arc::new(AvailableMcp::default()),
            Vec::new(),
        );

        holly
            .send(InMsg::McpList {
                correlation_id: "c1".into(),
            })
            .await
            .unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), sub.recv())
            .await
            .expect("timed out waiting for McpList reply")
            .unwrap();
        match ev {
            OutEvent::McpList {
                correlation_id,
                servers,
            } => {
                assert_eq!(correlation_id, "c1");
                assert!(servers.is_empty());
            }
            other => panic!("expected McpList, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn mcp_add_of_a_disabled_server_is_logged_not_replied() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let registry: SharedRegistry = Arc::new(RwLock::new(ToolRegistry::new()));
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));
        let handle = spawn_mcp_responder(
            &holly,
            registry.clone(),
            active.clone(),
            configs,
            Arc::new(AvailableMcp::default()),
            Vec::new(),
        );

        holly
            .send(InMsg::McpAdd {
                name: "srv".into(),
                config: McpServerSpec {
                    command: Some("definitely-not-a-real-binary-xyz".into()),
                    args: vec![],
                    env: HashMap::new(),
                    url: None,
                    headers: HashMap::new(),
                    disabled: true,
                },
            })
            .await
            .unwrap();

        // A failed/refused add never replies with McpChanged; confirm via a
        // McpList round-trip that nothing landed instead of racing a timeout.
        holly
            .send(InMsg::McpList {
                correlation_id: "check".into(),
            })
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), sub.recv())
                .await
                .expect("timed out")
                .unwrap()
            {
                OutEvent::McpChanged { .. } => panic!("a disabled server must not be added"),
                OutEvent::McpList {
                    correlation_id,
                    servers,
                } if correlation_id == "check" => {
                    assert!(servers.is_empty());
                    break;
                }
                _ => continue,
            }
        }
        assert!(registry.read().unwrap().is_empty());
        assert!(active.lock().unwrap().is_empty());
        handle.abort();
    }
}
