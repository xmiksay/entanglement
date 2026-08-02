//! Server connect machinery: startup [`connect`], the shared `connect_client`/
//! `connect_one`/`register_tools` helpers every connect path (startup, live
//! `/mcp add`, lazy `/enable mcp`) funnels through, and the small OAuth/
//! transport-label helpers connecting needs. Split out of `mcp/mod.rs` along
//! the 400-line file cap (#559, mirroring #556's `available_enable.rs` split)
//! — a `#[path]` sibling (not a real submodule boundary for privacy purposes:
//! items here are `pub(crate)` so every existing `super::`-qualified call site
//! in `live.rs`/`available_enable.rs`/`responder.rs` keeps compiling via
//! `mod.rs`'s re-export, unchanged).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::tools::{Tool, ToolRegistry};

use super::{
    ActiveServer, AvailableServer, McpClient, McpServerConfig, McpServerState, McpTool, McpToolDef,
};

/// `"stdio"` or `"http"`, for the [`McpServerStatus`][entanglement_core::McpServerStatus]
/// wire label — meaningful only after [`McpServerConfig::transport`] has already
/// validated the XOR, so it just reads back which field was set.
pub(crate) fn transport_label(cfg: &McpServerConfig) -> String {
    if cfg.command.is_some() {
        "stdio"
    } else {
        "http"
    }
    .to_string()
}

/// Connect to every configured server and register its tools into `registry`.
///
/// Best-effort per server: a connect/handshake/`tools/list` failure is logged and
/// skipped so one broken server can't stop startup. The registered [`McpTool`]s
/// hold an `Arc<McpClient>`, so keeping them in `registry` keeps each server
/// connection alive for the process lifetime — no separate handle to retain.
/// Returns the servers that connected, seeding [`ActiveServers`][super::ActiveServers]
/// (#375) so a later live `/mcp list`/`remove` sees exactly what startup
/// actually attached. `secret_env` (the catalog's provider API-key env vars,
/// #164) is scrubbed from every stdio server's child environment. `http` is
/// the shared endpoint pool (#559) every HTTP-transport server's traffic
/// rides; a bundled entry's `key_env` is resolved live so its pool-key
/// identity matches the LLM endpoint sharing the same provider key.
pub async fn connect(
    servers: &HashMap<String, AvailableServer>,
    registry: &mut ToolRegistry,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
) -> HashMap<String, ActiveServer> {
    let mut active = HashMap::new();
    for (name, entry) in servers {
        let cfg = &entry.config;
        if cfg.effective_state() != McpServerState::Enabled {
            tracing::info!("MCP server `{name}` is not enabled at startup; skipping");
            continue;
        }
        // An OAuth server with no stored credential is skipped *quietly* and
        // reported as "needs auth" by `/mcp list` (ADR-0153) — attempting the
        // connect would only produce a 401 and a scarier log line, and startup
        // must never open a browser (that is `/mcp connect`'s job).
        if needs_auth(name, cfg) {
            tracing::info!(
                "MCP server `{name}` needs authorization; run `/mcp connect {name}` to sign in"
            );
            continue;
        }
        let api_key = entry
            .key_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok());
        match connect_one(name, cfg, registry, secret_env, http, api_key.as_deref()).await {
            Ok((client, tools)) => {
                active.insert(
                    name.clone(),
                    ActiveServer {
                        client,
                        tools,
                        transport: transport_label(cfg),
                    },
                );
            }
            Err(e) => tracing::warn!("MCP server `{name}`: {e:#}"),
        }
    }
    active
}

/// The two network-I/O awaits (connect + `tools/list`), with no registry
/// involved — split out of the old single-shot `connect_one` (#375) so a live
/// `mcp_add` can run these *before* taking the registry's write lock, never
/// holding it across an `.await`. `http`/`api_key` (#559) are forwarded to the
/// HTTP transport so its traffic shares the endpoint pool a provider-bundled
/// server's key already budgets for LLM traffic; `api_key` is `None` for a
/// user-declared or OAuth-authenticated server (still pooled, just unkeyed).
pub(crate) async fn connect_client(
    name: &str,
    cfg: &McpServerConfig,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
    api_key: Option<&str>,
) -> Result<(Arc<McpClient>, Vec<McpToolDef>)> {
    #[cfg(feature = "mcp-http")]
    let client = McpClient::connect_with_auth(
        name,
        cfg,
        secret_env,
        oauth_token_source(name, cfg),
        http,
        api_key,
    )
    .await?;
    #[cfg(not(feature = "mcp-http"))]
    let client = McpClient::connect(name, cfg, secret_env, http, api_key).await?;
    let defs = client.list_tools().await?;
    Ok((client, defs))
}

/// The OAuth bearer source for a server that declares an `oauth:` block
/// (ADR-0153), or `None` for one authenticated by static headers (or not at
/// all) — in which case the transport behaves exactly as it did pre-ADR-0153.
///
/// The store is built per call rather than held in a process-wide handle: it is
/// only a resolved path (no I/O until a load), connects are rare, and building
/// it here keeps `ENTANGLEMENT_MCP_TOKENS_FILE` honoured at the moment of use.
#[cfg(feature = "mcp-http")]
fn oauth_token_source(
    name: &str,
    cfg: &McpServerConfig,
) -> Option<Arc<dyn entanglement_core::AccessTokenSource>> {
    cfg.oauth.as_ref()?;
    let store: Arc<dyn entanglement_core::TokenStore> =
        Arc::new(crate::config::McpTokenStore::load());
    Some(Arc::new(entanglement_core::StoredTokenSource::new(
        name, store,
    )))
}

/// Does this server want OAuth but have no stored credential yet (ADR-0153)?
///
/// Drives the "needs auth" state in `/mcp list` and the non-fatal startup skip:
/// an unauthenticated OAuth server is a normal, recoverable condition (run
/// `/mcp connect <name>`), not a transport failure worth alarming about.
pub fn needs_auth(name: &str, cfg: &McpServerConfig) -> bool {
    cfg.oauth.is_some() && !crate::config::McpTokenStore::load().has(name)
}

/// Register every discovered tool def into `registry`, synchronous (no
/// `.await`) so it is safe to run under a held write lock. Returns the
/// registered (already-namespaced) tool names.
pub(crate) fn register_tools(
    registry: &mut ToolRegistry,
    client: &Arc<McpClient>,
    name: &str,
    defs: Vec<McpToolDef>,
) -> Vec<String> {
    defs.into_iter()
        .map(|def| {
            let tool = McpTool::new(client.clone(), name, def);
            let tool_name = tool.name().into_owned();
            registry.register(tool);
            tool_name
        })
        .collect()
}

pub(crate) async fn connect_one(
    name: &str,
    cfg: &McpServerConfig,
    registry: &mut ToolRegistry,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
    api_key: Option<&str>,
) -> Result<(Arc<McpClient>, Vec<String>)> {
    let (client, defs) = connect_client(name, cfg, secret_env, http, api_key).await?;
    let count = defs.len();
    let tools = register_tools(registry, &client, name, defs);
    tracing::info!("MCP server `{name}`: registered {count} tool(s)");
    Ok((client, tools))
}
