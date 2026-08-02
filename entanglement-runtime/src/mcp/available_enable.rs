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
/// session. Returns the server's registered tool names.
pub async fn enable_for_session(
    avail: &AvailableMcp,
    name: &str,
    session: &SessionId,
    registry: &SharedRegistry,
    active: &ActiveServers,
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
                let (client, defs) = match tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    connect_client(name, &server.config, &avail.secret_env),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => bail!("connecting MCP server `{name}` timed out"),
                };
                let tools = {
                    let mut reg = registry.write().unwrap();
                    reg.unregister_prefix(&format!("mcp__{name}__"));
                    register_tools(&mut reg, &client, name, defs)
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
