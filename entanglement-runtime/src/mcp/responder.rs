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

use entanglement_core::{Holly, InMsg, McpAction, McpAuthAction, McpAuthStatus, McpServerStatus};
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
/// `http` (#559) is the shared endpoint pool every live add/reconnect's HTTP
/// transport rides.
pub fn spawn_mcp_responder(
    holly: &Holly,
    registry: SharedRegistry,
    active: ActiveServers,
    configs: ServerConfigs,
    avail: Arc<AvailableMcp>,
    secret_env: Vec<String>,
    http: entanglement_core::HttpClient,
) -> tokio::task::JoinHandle<()> {
    let emitter = holly.clone();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        // Tracks the detached `McpAuth` ops below so they're reachable at
        // shutdown (#545): a `Connect` can hold its own `Holly` clone (`emitter`)
        // for up to five minutes waiting on the user's browser, and aborting
        // *this* task never reaches a child it already spawned via a bare
        // `tokio::spawn` — only dropping this `JoinSet` does. Each task
        // resolves to the server name it ran for, so completion can clear
        // `in_flight_auth` below (#556).
        let mut auth_ops: tokio::task::JoinSet<String> = tokio::task::JoinSet::new();
        // Servers with an auth op currently running (#556): two concurrent
        // `McpAuth{Connect}` for the same server would otherwise both open a
        // loopback listener, both start a DCR, and both pop a browser tab —
        // the later request is dropped instead, not queued or double-run.
        let mut in_flight_auth: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        loop {
            tokio::select! {
                // Reap finished auth ops so `auth_ops` doesn't grow unbounded;
                // guarded so this branch never busy-loops while empty.
                Some(done) = auth_ops.join_next(), if !auth_ops.is_empty() => {
                    if let Ok(name) = done {
                        in_flight_auth.remove(&name);
                    }
                }
                msg = inbound.recv() => {
                    match msg {
                        Ok(InMsg::McpList { correlation_id }) => {
                            let servers = full_list(&active, &avail, &configs);
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
                                &http,
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
                            // A bundled/`allowed` server (#542) has no config-map
                            // entry to remove — disconnect it (it stays
                            // *available*) instead of erroring halfway through
                            // `mcp_remove`.
                            if avail.is_lazy(&name) || avail.get(&name).is_some() {
                                super::available::disconnect(&avail, &name, &registry, &active);
                                emitter.emit_mcp_changed(name, McpAction::Removed);
                            } else {
                                match mcp_remove(&name, &registry, &active, &configs) {
                                    Ok(()) => emitter.emit_mcp_changed(name, McpAction::Removed),
                                    Err(e) => {
                                        tracing::warn!(server = %name, "MCP remove failed: {e:#}")
                                    }
                                }
                            }
                        }
                        Ok(InMsg::McpAuth { name, action }) => {
                            // In-flight dedupe (#556): a second `McpAuth` for a
                            // server already mid-op is dropped rather than
                            // racing it — two loopback listeners/DCRs/browser
                            // tabs for the same server would only confuse the
                            // user and leave one flow's credential save racing
                            // the other's.
                            if !in_flight_auth.insert(name.clone()) {
                                tracing::debug!(
                                    server = %name,
                                    "MCP auth op already in flight — ignoring duplicate request"
                                );
                                continue;
                            }
                            // Tracked (not bare-detached, #545): a `Connect`
                            // parks for up to five minutes on the user's
                            // browser, and blocking the responder would stall
                            // every later `/mcp` op (and every other MCP frame)
                            // behind it. Each op is independent, so concurrency
                            // is safe; `auth_ops` keeps it reachable for abort
                            // at shutdown instead of leaking its `Holly` clone.
                            let emitter = emitter.clone();
                            let (registry, active, configs) =
                                (registry.clone(), active.clone(), configs.clone());
                            let secret_env = secret_env.clone();
                            let http = http.clone();
                            let op_name = name.clone();
                            auth_ops.spawn(async move {
                                run_auth_op(
                                    &emitter,
                                    name,
                                    action,
                                    &registry,
                                    &active,
                                    &configs,
                                    &secret_env,
                                    &http,
                                )
                                .await;
                                op_name
                            });
                        }
                        Ok(_) => {}
                        // A dropped inbound frame under lag can only lose a
                        // query/command — the head times out and re-asks; keep
                        // serving.
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!("MCP responder lagged, skipped {n} inbound messages");
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            }
        }
    })
}

/// Run one MCP OAuth op and report it (ADR-0153).
///
/// A `Connect` emits twice: an interim `McpAuthChanged` carrying the
/// authorization URL (so the head can show it while the browser is open —
/// essential when the launch failed or there is no browser at all), then the
/// terminal outcome. `Check`/`Disconnect` emit once.
///
/// A failure is reported as `error` on the same event rather than logged and
/// dropped: unlike `McpAdd`/`McpRemove` (whose failures are diagnostic), the
/// user is *waiting* on this one, and silence would read as a hang.
#[allow(clippy::too_many_arguments)]
async fn run_auth_op(
    emitter: &Holly,
    name: String,
    action: McpAuthAction,
    registry: &SharedRegistry,
    active: &ActiveServers,
    configs: &ServerConfigs,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
) {
    let report = |status| emitter.emit_mcp_auth_changed(status);
    let interim_name = name.clone();
    let interim_emitter = emitter.clone();
    let result = super::oauth_ops::run(
        &name,
        action,
        configs,
        registry,
        active,
        secret_env,
        http,
        move |pending| {
            if !pending.browser_opened {
                tracing::info!(
                    server = %interim_name,
                    "could not open a browser; open this URL to authorize: {}",
                    pending.url
                );
            }
            interim_emitter.emit_mcp_auth_changed(McpAuthStatus {
                name: interim_name,
                action,
                state: None,
                error: None,
                authorize_url: Some(pending.url),
            });
        },
    )
    .await;

    match result {
        Ok(outcome) => {
            tracing::info!(server = %name, "MCP auth: {}", outcome.as_str());
            report(McpAuthStatus {
                name,
                action,
                state: Some(outcome.as_str().to_string()),
                error: None,
                authorize_url: None,
            });
        }
        Err(e) => {
            tracing::warn!(server = %name, "MCP auth failed: {e:#}");
            report(McpAuthStatus {
                name,
                action,
                state: None,
                // `{e:#}` is the whole context chain; no token value can appear
                // in it (the provider redacts secrets from every error path).
                error: Some(format!("{e:#}")),
                authorize_url: None,
            });
        }
    }
}

/// The `McpList` snapshot (#542): every connected server (state `enabled`,
/// or `allowed` when it was lazily connected) plus every available-but-
/// unconnected `allowed` server — keyless bundles silently absent. Sorted by
/// name for stable output.
fn full_list(
    active: &ActiveServers,
    avail: &AvailableMcp,
    configs: &ServerConfigs,
) -> Vec<McpServerStatus> {
    let mut list = mcp_list(active);
    for s in &mut list {
        let state = if avail.is_lazy(&s.name) {
            "allowed"
        } else {
            "enabled"
        };
        s.state = Some(state.to_string());
    }
    // OAuth servers still awaiting sign-in (ADR-0153) are configured but
    // neither connected nor "available", so they would otherwise be missing
    // from the listing entirely — precisely the case the user needs to see.
    for (name, cfg) in configs.lock().unwrap().iter() {
        if cfg.oauth.is_none() {
            continue;
        }
        match list.iter_mut().find(|s| &s.name == name) {
            Some(existing) => existing.auth = Some("authenticated".to_string()),
            None => list.push(McpServerStatus {
                name: name.clone(),
                transport: transport_label(cfg),
                connected: false,
                tools: Vec::new(),
                error: None,
                state: Some("enabled".to_string()),
                auth: Some("needs auth".to_string()),
            }),
        }
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
            auth: None,
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
                oauth: None,
                state: Some(crate::mcp::McpServerState::Allowed),
            },
        );
        let (_, avail) = AvailableMcp::partition(
            &entanglement_core::Catalog { providers: vec![] },
            &user,
            vec![],
        );
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));
        let list = full_list(&active, &avail, &configs);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "mine");
        assert!(!list[0].connected);
        assert_eq!(list[0].state.as_deref(), Some("allowed"));
        assert_eq!(list[0].transport, "http");
        assert!(list[0].tools.is_empty());
        // Not an OAuth server → no auth posture reported.
        assert!(list[0].auth.is_none());
    }

    /// An OAuth server with no stored credential is skipped at startup, so it
    /// is neither connected nor "available" — `full_list` must still surface it
    /// as `needs auth` (ADR-0153), or the user has no way to discover that
    /// `/mcp connect` is what's missing.
    #[test]
    fn full_list_surfaces_an_unauthenticated_oauth_server() {
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let (_, avail) = AvailableMcp::partition(
            &entanglement_core::Catalog { providers: vec![] },
            &HashMap::new(),
            vec![],
        );
        let mut cfgs = HashMap::new();
        cfgs.insert(
            "remote".to_string(),
            crate::mcp::McpServerConfig {
                command: None,
                args: vec![],
                env: HashMap::new(),
                url: Some("https://example.com/mcp".into()),
                headers: HashMap::new(),
                disabled: false,
                capabilities: HashMap::new(),
                oauth: Some(Default::default()),
                state: None,
            },
        );
        let configs: ServerConfigs = Arc::new(Mutex::new(cfgs));
        let list = full_list(&active, &avail, &configs);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "remote");
        assert!(!list[0].connected);
        assert_eq!(list[0].auth.as_deref(), Some("needs auth"));
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
            entanglement_core::HttpClient::default(),
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
            entanglement_core::HttpClient::default(),
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
