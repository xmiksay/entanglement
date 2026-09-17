//! `enable_for_session`/`disconnect` for [`super::AvailableMcp`] (#542). Split
//! out of `available.rs` along the 400-line file cap (#556) — the lazy-connect
//! critical section grew a per-server guard + timeout and pushed the file
//! over. Sibling `#[path]` child module (not `available_tests.rs`'s sibling
//! test file), so private fields/methods stay reachable via the normal
//! descendant-module visibility rule.

use std::time::Duration;

use anyhow::{bail, Result};
use entanglement_core::SessionId;

use crate::tools::SharedRegistry;

use super::super::live::{ActiveServer, ActiveServers};
use super::super::{connect_client, register_tools, transport_label};
use super::AvailableMcp;

/// Ceiling on a lazy `mcp_enable` connect (#556): a hung server must not park
/// the calling turn — nor every other caller waiting on the per-server
/// [`AvailableMcp::connect_guard`] — forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Enable an available server for `session` (#542): lazily connect + register
/// its tools if this is the first enablement (no persistence — `ServerConfigs`
/// and `config.yml` are never touched, unlike `mcp_add`), then mark the
/// session. Returns the server's registered tool names. `http` (#559) is the
/// shared endpoint pool the connect rides — a bundled server's own `key_env`
/// (from its `AvailableServer` entry) is resolved live so its traffic shares
/// pool-key identity with the LLM endpoint billed against the same key.
pub async fn enable_for_session(
    avail: &AvailableMcp,
    name: &str,
    session: &SessionId,
    registry: &SharedRegistry,
    active: &ActiveServers,
    http: &entanglement_core::HttpClient,
) -> Result<Vec<String>> {
    let connected = active.lock().unwrap().get(name).map(|s| s.tools.clone());
    // A startup-`enabled` server is globally visible already — enabling it
    // must *not* mark it lazy, or the spec filter would suddenly scope it to
    // this session and hide it everywhere else.
    if !avail.is_lazy(name) && avail.get(name).is_none() {
        return match connected {
            Some(tools) => Ok(tools),
            None => bail!(
                "no available MCP server named `{name}` (available: {})",
                avail.available_names().join(", ")
            ),
        };
    }
    let tools = match connected {
        Some(tools) => tools,
        None => {
            // Serialize the check-then-connect-then-register sequence per
            // server (#556 TOCTOU): two concurrent enables of a not-yet-
            // connected server must not both pass the check above,
            // double-connect, and orphan the loser's registered tools.
            let guard = avail.connect_guard(name);
            let _permit = guard.lock().await;
            // Re-check now that we hold the guard — another caller may have
            // finished connecting while we waited for it. Bound to a `let`
            // first so the `MutexGuard` drops before the `else` branch's
            // `.await`s (an `if let`'s scrutinee temporary otherwise lives
            // for the whole if/else, poisoning the future's `Send`-ness).
            let already_connected = active.lock().unwrap().get(name).map(|s| s.tools.clone());
            if let Some(tools) = already_connected {
                tools
            } else {
                let Some(server) = avail.get(name) else {
                    bail!(
                        "no available MCP server named `{name}` (available: {})",
                        avail.available_names().join(", ")
                    );
                };
                let api_key = server
                    .key_env
                    .as_deref()
                    .and_then(|var| std::env::var(var).ok());
                let (client, defs) = match tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    connect_client(
                        name,
                        &server.config,
                        &avail.secret_env,
                        http,
                        api_key.as_deref(),
                        None,
                    ),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => bail!("connecting MCP server `{name}` timed out"),
                };
                let tools = {
                    let mut reg = registry.write().unwrap();
                    reg.unregister_prefix(&format!("mcp__{name}__"));
                    register_tools(&mut reg, &client, name, defs, &server.config.capabilities)
                };
                active.lock().unwrap().insert(
                    name.to_string(),
                    ActiveServer {
                        client,
                        tools: tools.clone(),
                        transport: transport_label(&server.config),
                    },
                );
                tracing::info!(
                    "MCP server `{name}`: lazily connected, {} tool(s)",
                    tools.len()
                );
                tools
            }
        }
    };
    avail.mark_enabled(name, session);
    Ok(tools)
}

/// Cooldown window for [`try_lazy_reenable`]'s stampede guard (ADR-0201): a
/// dispatch-time re-enable failure within this window short-circuits a later
/// unknown `mcp__<server>__*` call against the same server to the same error
/// instead of each retrying the full [`CONNECT_TIMEOUT`] connect.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// How a dispatch-time "unknown tool" resolution for an `mcp__<server>__*`
/// name settled (ADR-0201) — see [`try_lazy_reenable`].
pub enum LazyReenableOutcome {
    /// [`super::McpTier::Unknown`]: no registered tool, no configured/
    /// bundled server under this name in any tier — the caller falls
    /// through to the ordinary unknown-tool message, unchanged.
    Unknown,
    /// [`super::McpTier::Disabled`]: the server is known but explicitly
    /// configured off — a truthful, attributed decline
    /// ([`super::disabled_decline`]), never the unknown-tool message.
    Disabled,
    /// Re-enabled successfully via the `allowed`/lazy tier — the same
    /// zero-approval path `mcp_enable`/`/enable mcp` use (ADR-0152: the tier
    /// itself is the consent boundary, not a fresh approval prompt). The
    /// caller must re-fetch its registry snapshot (this registered the
    /// tools into the *live* registry, invisible to an already-cloned one)
    /// and continue the dispatch ladder exactly as if the tool had been
    /// registered all along.
    Enabled,
    /// The connect failed — fresh, or short-circuited by the
    /// [`FAILURE_COOLDOWN`] guard with the same message.
    Failed(String),
}

/// Self-heal a dispatch-time "unknown tool" for an `mcp__<server>__*` name
/// whose server isn't currently registered (ADR-0201) — the resume-
/// coherence gap: a resumed session's replay restores its tool-overlay/
/// permission state (so the call is never masked) but never re-registers a
/// bundled server's tools — `ToolRegistry` is process-lifetime, never
/// persisted, and nothing re-enables on resume. Classifies `server` per
/// [`AvailableMcp::tier_of`] first — exactly the check `mcp_enable`/
/// `/enable mcp` make — so a `disabled` tier or a truly unknown name is
/// answered truthfully rather than lazily connected or misreported as
/// unknown. `http: None` (no HTTP client wired into the caller) is treated
/// as a connect failure, never a panic — every in-tree head wires one; only
/// a hand-rolled embedder omitting `DiscoverySurface::http` while still
/// populating `mcp_avail` would ever see it.
pub async fn try_lazy_reenable(
    avail: &AvailableMcp,
    server: &str,
    session: &SessionId,
    registry: &SharedRegistry,
    active: &ActiveServers,
    http: Option<&entanglement_core::HttpClient>,
) -> LazyReenableOutcome {
    match avail.tier_of(server) {
        super::McpTier::Unknown => return LazyReenableOutcome::Unknown,
        super::McpTier::Disabled => return LazyReenableOutcome::Disabled,
        super::McpTier::Eligible => {}
    }
    if avail.recently_failed_enable(server, FAILURE_COOLDOWN) {
        return LazyReenableOutcome::Failed(format!(
            "mcp server `{server}` could not be re-enabled: recent connect failure, retry shortly"
        ));
    }
    let Some(http) = http else {
        return LazyReenableOutcome::Failed(format!(
            "mcp server `{server}` could not be re-enabled: no HTTP client configured for lazy \
             MCP re-enablement"
        ));
    };
    match enable_for_session(avail, server, session, registry, active, http).await {
        Ok(_tools) => {
            avail.clear_enable_failure(server);
            tracing::info!(
                %session,
                server,
                "MCP server: dispatch-time lazy re-enable (resume self-heal, ADR-0201)"
            );
            LazyReenableOutcome::Enabled
        }
        Err(e) => {
            avail.record_enable_failure(server);
            LazyReenableOutcome::Failed(format!(
                "mcp server `{server}` could not be re-enabled: {e:#}"
            ))
        }
    }
}

/// Disconnect a lazily-connected available server entirely (the `/mcp remove`
/// arm for a bundled name): unregister its tools, drop the connection, clear
/// every session's enablement mark. Nothing is persisted — the server stays
/// *available* and can be re-enabled.
pub fn disconnect(
    avail: &AvailableMcp,
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
) {
    registry
        .write()
        .unwrap()
        .unregister_prefix(&format!("mcp__{name}__"));
    active.lock().unwrap().remove(name);
    avail.enabled.lock().unwrap().remove(name);
    tracing::info!("MCP server `{name}`: disconnected (still available)");
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, RwLock};

    use super::*;

    fn registry() -> SharedRegistry {
        Arc::new(RwLock::new(crate::tools::ToolRegistry::new()))
    }
    fn active() -> ActiveServers {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// [`super::McpTier::Unknown`] short-circuits before any connect
    /// attempt — no `http` client is even consulted.
    #[tokio::test]
    async fn unknown_server_short_circuits_without_a_connect_attempt() {
        let avail = AvailableMcp::default();
        let out = try_lazy_reenable(
            &avail,
            "nope",
            &SessionId::new("s"),
            &registry(),
            &active(),
            None,
        )
        .await;
        assert!(matches!(out, LazyReenableOutcome::Unknown));
    }

    /// [`super::McpTier::Disabled`] likewise short-circuits — a known name,
    /// truthfully declined, never treated as eligible for a connect.
    #[tokio::test]
    async fn disabled_server_short_circuits_without_a_connect_attempt() {
        let mut avail = AvailableMcp::default();
        avail.disabled_names.insert("off".to_string());
        let out = try_lazy_reenable(
            &avail,
            "off",
            &SessionId::new("s"),
            &registry(),
            &active(),
            None,
        )
        .await;
        assert!(matches!(out, LazyReenableOutcome::Disabled));
    }

    /// A tier-eligible server with no `http` client wired (`DiscoverySurface
    /// { http: None, .. }`) fails cleanly — never panics — and says why.
    #[tokio::test]
    async fn eligible_with_no_http_client_fails_cleanly_not_a_panic() {
        let avail = AvailableMcp::default();
        avail.mark_enabled("lazy", &SessionId::new("s0"));
        let out = try_lazy_reenable(
            &avail,
            "lazy",
            &SessionId::new("s"),
            &registry(),
            &active(),
            None,
        )
        .await;
        match out {
            LazyReenableOutcome::Failed(msg) => assert!(msg.contains("no HTTP client"), "{msg}"),
            _ => panic!("expected a Failed outcome"),
        }
    }
}
