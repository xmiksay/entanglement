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
//! A refused or failed `McpAdd`/`McpRemove` is logged and reported as an
//! `OutEvent::McpChanged` carrying `error` — engine-global like the success
//! reply, since there is no session to attach an `Error` to — so the head that
//! asked learns why nothing changed.

use std::sync::Arc;

use entanglement_core::{Holly, InMsg, McpAction, McpAuthAction, McpAuthStatus, McpServerStatus};
use tokio::sync::broadcast::error::RecvError;

use crate::tools::SharedRegistry;

use super::available::{self, AvailableMcp};
use super::live::{mcp_add, mcp_remove, ActiveServers, ServerConfigs};

/// Spawns a subscriber that answers `InMsg::McpList`/`McpAdd`/`McpRemove`.
/// `secret_env` (the catalog's provider API-key env vars, #164) is scrubbed
/// from any stdio server a live add spawns. `avail` is the server roster
/// (ADR-0226): `McpList` lists every registered server, a live add/remove
/// keeps it in step, and a `McpRemove` of a bundled server is a plain
/// disconnect (it stays available) instead of a config-map removal. `http`
/// (#559) is the shared endpoint pool every live add/reconnect rides.
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
                            let servers = full_list(&active, &avail);
                            emitter.emit_mcp_list(correlation_id, servers);
                        }
                        Ok(InMsg::McpAdd { name, config }) => {
                            if let Some(other) = avail.clashing_name(&name) {
                                let why = format!(
                                    "its tool names would collide with server `{other}`'s \
                                     — rename one"
                                );
                                tracing::warn!(server = %name, "MCP add refused: {why}");
                                emitter.emit_mcp_change_failed(name, McpAction::Added, why);
                                continue;
                            }
                            let config: super::McpServerConfig = config.into();
                            match mcp_add(
                                name.clone(),
                                config.clone(),
                                &registry,
                                &active,
                                &configs,
                                &secret_env,
                                &http,
                            )
                            .await
                            {
                                Ok(tools) => {
                                    super::record_last_seen(
                                        avail.tool_cache(),
                                        &name,
                                        &config,
                                        &tools,
                                    )
                                    .await;
                                    avail.insert_user(&name, config);
                                    tracing::info!(server = %name, tools = tools.len(), "MCP: live-added");
                                    emitter.emit_mcp_changed(name, McpAction::Added);
                                }
                                Err(e) => {
                                    tracing::warn!(server = %name, "MCP add failed: {e:#}");
                                    let why = format!("{e:#}");
                                    emitter.emit_mcp_change_failed(name, McpAction::Added, why);
                                }
                            }
                        }
                        Ok(InMsg::McpRemove { name }) => {
                            // A bundled server (#542) has no config-map entry to
                            // remove — disconnect it (it stays available)
                            // instead of erroring halfway through `mcp_remove`.
                            if avail.is_bundled(&name) {
                                available::disconnect(&name, &registry, &active);
                                emitter.emit_mcp_changed(name, McpAction::Removed);
                            } else {
                                match mcp_remove(&name, &registry, &active, &configs) {
                                    Ok(()) => {
                                        avail.remove(&name);
                                        emitter.emit_mcp_changed(name, McpAction::Removed)
                                    }
                                    Err(e) => {
                                        tracing::warn!(server = %name, "MCP remove failed: {e:#}");
                                        let why = format!("{e:#}");
                                        emitter.emit_mcp_change_failed(
                                            name,
                                            McpAction::Removed,
                                            why,
                                        );
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
                            let (registry, active, configs, avail) =
                                (registry.clone(), active.clone(), configs.clone(), avail.clone());
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
                                    &avail,
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
    avail: &AvailableMcp,
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
                match &pending.user_code {
                    Some(code) => tracing::info!(
                        server = %interim_name,
                        "could not open a browser; open this URL and enter code {code} to \
                         authorize: {}",
                        pending.url
                    ),
                    None => tracing::info!(
                        server = %interim_name,
                        "could not open a browser; open this URL to authorize: {}",
                        pending.url
                    ),
                }
            }
            interim_emitter.emit_mcp_auth_changed(McpAuthStatus {
                name: interim_name,
                action,
                state: None,
                error: None,
                authorize_url: Some(pending.url),
                user_code: pending.user_code.clone(),
            });
        },
    )
    .await;

    match result {
        Ok(outcome) => {
            tracing::info!(server = %name, "MCP auth: {}", outcome.as_str());
            // A `Connect` reconnected the server: record its tools like any
            // other connect so the next process lists them as last seen.
            let tools = active
                .lock()
                .expect("MCP active-server mutex poisoned")
                .get(&name)
                .map(|s| s.tools.clone());
            if let (Some(tools), Some(server)) = (tools, avail.get(&name)) {
                avail.clear_connect_failure(&name);
                super::record_last_seen(avail.tool_cache(), &name, &server.config, &tools).await;
            }
            report(McpAuthStatus {
                name,
                action,
                state: Some(outcome.as_str().to_string()),
                error: None,
                authorize_url: None,
                user_code: None,
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
                user_code: None,
            });
        }
    }
}

/// The `McpList` snapshot: every registered server (ADR-0226), connected or
/// not, with its OAuth posture (ADR-0153) — an unauthenticated server is
/// listed `needs auth` so the user learns `/mcp connect` is what's missing.
/// `state` is retired and left `None` (kept on the wire for old logs).
fn full_list(active: &ActiveServers, avail: &AvailableMcp) -> Vec<McpServerStatus> {
    super::roster(avail, active)
        .into_iter()
        .map(|row| McpServerStatus {
            auth: row.oauth.then(|| {
                if row.auth_required {
                    "needs auth".to_string()
                } else {
                    "authenticated".to_string()
                }
            }),
            name: row.name,
            transport: row.transport,
            connected: row.connected,
            tools: row.tools,
            error: None,
            state: None,
        })
        .collect()
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

    fn http_cfg(oauth: bool) -> crate::mcp::McpServerConfig {
        crate::mcp::McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some("https://example.com/mcp".into()),
            headers: HashMap::new(),
            disabled: false,
            capabilities: HashMap::new(),
            oauth: oauth.then(Default::default),
            description: None,
        }
    }

    /// ADR-0226: a registered, unconnected server rides the `McpList`
    /// snapshot with `connected: false` and no retired `state`.
    #[test]
    fn full_list_includes_unconnected_servers() {
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let avail = AvailableMcp::default();
        avail.insert_user("mine", http_cfg(false));
        let list = full_list(&active, &avail);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "mine");
        assert!(!list[0].connected);
        assert_eq!(list[0].state, None);
        assert_eq!(list[0].transport, "http");
        assert!(list[0].tools.is_empty());
        // Not an OAuth server → no auth posture reported.
        assert!(list[0].auth.is_none());
    }

    /// An OAuth server with no stored credential lists as `needs auth`
    /// (ADR-0153), so the user learns `/mcp connect` is what's missing.
    #[test]
    fn full_list_surfaces_an_unauthenticated_oauth_server() {
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let avail =
            AvailableMcp::default().with_token_store(crate::config::McpTokenStore::at(None));
        avail.insert_user("remote-0226-unauthed", http_cfg(true));
        let list = full_list(&active, &avail);
        assert_eq!(list.len(), 1);
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
            crate::test_http_client(),
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

    /// The next `McpChanged` the responder emits, as `(name, error)`.
    async fn next_change(
        sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    ) -> (String, Option<String>) {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
                .await
                .expect("timed out waiting for McpChanged")
                .unwrap()
            {
                OutEvent::McpChanged { name, error, .. } => return (name, error),
                _ => continue,
            }
        }
    }

    fn spec(disabled: bool) -> McpServerSpec {
        McpServerSpec {
            command: Some("definitely-not-a-real-binary-xyz".into()),
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            disabled,
        }
    }

    /// A refused (disabled) add replies with an `McpChanged` carrying the
    /// reason, and nothing lands.
    #[tokio::test]
    async fn a_refused_mcp_add_replies_with_its_error() {
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
            crate::test_http_client(),
        );
        holly
            .send(InMsg::McpAdd {
                name: "srv".into(),
                config: spec(true),
            })
            .await
            .unwrap();
        let (name, error) = next_change(&mut sub).await;
        assert_eq!(name, "srv");
        assert!(
            error.as_deref().unwrap_or("").contains("disabled"),
            "{error:?}"
        );
        assert!(registry.read().unwrap().is_empty());
        assert!(active.lock().unwrap().is_empty());
        handle.abort();
    }

    /// A live add whose name clashes with a registered server's tool names
    /// (`my_docs` vs `my.docs`) is refused before any connect, with the reason
    /// on the wire.
    #[tokio::test]
    async fn a_clashing_mcp_add_is_refused_on_the_wire() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let avail = AvailableMcp::default();
        avail.insert_user("my.docs", crate::mcp::available::tests::stdio("true"));
        let handle = spawn_mcp_responder(
            &holly,
            Arc::new(RwLock::new(ToolRegistry::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(avail),
            Vec::new(),
            crate::test_http_client(),
        );
        holly
            .send(InMsg::McpAdd {
                name: "my_docs".into(),
                config: spec(false),
            })
            .await
            .unwrap();
        let (name, error) = next_change(&mut sub).await;
        assert_eq!(name, "my_docs");
        assert!(
            error.as_deref().unwrap_or("").contains("`my.docs`"),
            "{error:?}"
        );
        handle.abort();
    }
}
