//! Server connect machinery: the shared `connect_client`/`register_tools`
//! helpers every connect path (lazy connect, live `/mcp add`, `/mcp connect`)
//! funnels through, the last-seen tool-name record, and the small
//! OAuth/transport-label helpers connecting needs. No startup connect exists
//! any more (ADR-0226). Split out of `mcp/mod.rs` along the 400-line file cap
//! (#559) — items are `pub(crate)` and re-exported by `mod.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::tools::{Tool, ToolRegistry};

use super::{McpClient, McpServerConfig, McpTool, McpToolCache, McpToolDef};

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
    connect_client_with_store(name, cfg, secret_env, http, api_key, None).await
}

/// [`connect_client`] with the OAuth credential source made explicit (#684):
/// `token_store` is the per-scope store a session-keyed MCP scope supplies
/// ([`super::scoped::McpScopes`]), and `None` falls back to the process-global
/// managed file — byte-identical to every pre-#684 caller.
pub(crate) async fn connect_client_with_store(
    name: &str,
    cfg: &McpServerConfig,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
    api_key: Option<&str>,
    #[cfg_attr(not(feature = "mcp-http"), allow(unused_variables))] token_store: Option<
        Arc<dyn entanglement_core::TokenStore>,
    >,
) -> Result<(Arc<McpClient>, Vec<McpToolDef>)> {
    #[cfg(feature = "mcp-http")]
    let client = McpClient::connect_with_auth(
        name,
        cfg,
        secret_env,
        oauth_token_source(name, cfg, token_store),
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
/// `store` is a per-scope credential slice when the connect belongs to a
/// session-keyed MCP scope (#684); absent, the process-global managed file.
///
/// The global store is built per call rather than held in a process-wide
/// handle: it is only a resolved path (no I/O until a load), connects are rare,
/// and building it here keeps `ENTANGLEMENT_STATE_DIR` honoured at the
/// moment of use.
#[cfg(feature = "mcp-http")]
fn oauth_token_source(
    name: &str,
    cfg: &McpServerConfig,
    store: Option<Arc<dyn entanglement_core::TokenStore>>,
) -> Option<Arc<dyn entanglement_core::AccessTokenSource>> {
    cfg.oauth.as_ref()?;
    let store: Arc<dyn entanglement_core::TokenStore> =
        store.unwrap_or_else(|| Arc::new(crate::config::McpTokenStore::load()));
    Some(Arc::new(entanglement_core::StoredTokenSource::new(
        name, store,
    )))
}

/// Register every discovered tool def into `registry`, synchronous (no
/// `.await`) so it is safe to run under a held write lock. Returns the
/// registered (already-namespaced) tool names. `capabilities` is the
/// server's own config-side `capabilities:` annotation map (ADR-0117),
/// forwarded straight to [`McpTool::new`] so each registered tool grades by
/// its declared read/write/call capability instead of the old fail-safe-only
/// `Write` (ADR-0207 §3).
pub(crate) fn register_tools(
    registry: &mut ToolRegistry,
    client: &Arc<McpClient>,
    name: &str,
    defs: Vec<McpToolDef>,
    capabilities: &HashMap<String, String>,
) -> Vec<String> {
    defs.into_iter()
        .map(|def| {
            let tool = McpTool::new(client.clone(), name, def, capabilities);
            let tool_name = tool.name().into_owned();
            registry.register(tool);
            tool_name
        })
        .collect()
}

/// Record `server`'s freshly registered `tools` in `cache`, as the bare names
/// `list_mcp` shows — on the blocking pool, since the write takes a file
/// lock. A failure only costs the next process a tool list for an
/// unconnected server, so it is logged, never surfaced.
pub(crate) async fn record_last_seen(
    cache: &McpToolCache,
    server: &str,
    cfg: &McpServerConfig,
    tools: &[String],
) {
    let prefix = super::tool::namespaced_tool_name(server, "");
    let names: Vec<String> = tools
        .iter()
        .map(|t| t.strip_prefix(&prefix).unwrap_or(t).to_string())
        .collect();
    let (cache, server, target) = (cache.clone(), server.to_string(), cfg.target());
    let written = tokio::task::spawn_blocking(move || cache.record(&server, &target, &names)).await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("recording MCP tool names failed: {e:#}"),
        Err(e) => tracing::warn!("recording MCP tool names panicked: {e}"),
    }
}
