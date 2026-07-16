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

use entanglement_core::{Holly, InMsg, McpAction};
use tokio::sync::broadcast::error::RecvError;

use crate::tools::SharedRegistry;

use super::live::{mcp_add, mcp_list, mcp_remove, ActiveServers, ServerConfigs};

/// Spawns a subscriber that answers `InMsg::McpList`/`McpAdd`/`McpRemove`.
pub fn spawn_mcp_responder(
    holly: &Holly,
    registry: SharedRegistry,
    active: ActiveServers,
    configs: ServerConfigs,
) -> tokio::task::JoinHandle<()> {
    let emitter = holly.clone();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        loop {
            match inbound.recv().await {
                Ok(InMsg::McpList { correlation_id }) => {
                    let servers = mcp_list(&active);
                    emitter.emit_mcp_list(correlation_id, servers);
                }
                Ok(InMsg::McpAdd { name, config }) => {
                    match mcp_add(name.clone(), config.into(), &registry, &active, &configs).await {
                        Ok(tools) => {
                            tracing::info!(server = %name, tools = tools.len(), "MCP: live-added");
                            emitter.emit_mcp_changed(name, McpAction::Added);
                        }
                        Err(e) => tracing::warn!(server = %name, "MCP add failed: {e:#}"),
                    }
                }
                Ok(InMsg::McpRemove { name }) => {
                    match mcp_remove(&name, &registry, &active, &configs) {
                        Ok(()) => emitter.emit_mcp_changed(name, McpAction::Removed),
                        Err(e) => tracing::warn!(server = %name, "MCP remove failed: {e:#}"),
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

    #[tokio::test]
    async fn mcp_list_replies_with_an_empty_snapshot() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let registry: SharedRegistry = Arc::new(RwLock::new(ToolRegistry::new()));
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));
        let handle = spawn_mcp_responder(&holly, registry, active, configs);

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
        let handle = spawn_mcp_responder(&holly, registry.clone(), active.clone(), configs);

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
