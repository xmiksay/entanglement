//! Live MCP server management (#375, Phase 4 of the MCP umbrella): hot-add/
//! remove a server in the running process (#932). Nothing here writes a
//! file: the `McpChanged` the responder emits is what
//! `state::mcp_config` persists to `config.yml`'s `mcp:` section.
//!
//! `ServerConfigs` is the live mirror of every *configured* server
//! (including one that failed to connect or is `disabled`) — wider than the
//! library's `ActiveServers` — the full set written back, since it replaces
//! the whole `mcp:` section. Losing track of a never-connected entry there
//! would silently drop it from the config on the next live add/remove.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use entanglement_core::TokenStore;

use crate::tools::SharedRegistry;

use super::{connect_client, drop_server, install_server, ActiveServers, McpServerConfig};

/// The live mirror of every *configured* server — wider than [`ActiveServers`]
/// (includes `disabled`/failed-to-connect entries) — so a head persisting it
/// after one add/remove never drops an unrelated entry. Seeded at startup from
/// the user config's `mcp:` section.
pub type ServerConfigs = Arc<Mutex<HashMap<String, McpServerConfig>>>;

/// Hot-connect a server: `connect` → register its tools into `registry` →
/// track it in `active` → record it in `configs`.
///
/// Upsert semantics: re-adding an already-active name first drops its old
/// tools/connection (unregister + let the old `Arc<McpClient>` drop), so
/// reconfiguring a broken server — or one that failed at startup — cleanly
/// replaces it instead of leaking the old connection. `cfg.disabled` is
/// refused: a disabled server has nothing to connect live.
///
/// `headers`/`env` left empty carry forward from the existing entry, if any
/// (#561): the wire spec has no way to say "I didn't touch this field", so an
/// empty map from `/mcp add <name> --url ...` re-adding a server *without*
/// `--header` means "unspecified", not "clear the token" — there is no flag to
/// explicitly blank a header. Silently replacing a bearer-authed entry with a
/// headerless one reads as "auth stopped working" once the server reconnects
/// with its anonymous (fewer-tool) surface.
///
/// The connect/list-tools awaits run *before* any lock is taken (#372: never
/// hold a lock across `.await`); only the synchronous registration is done
/// under the registry's write lock. `http` (#559) is the shared endpoint pool
/// the connect rides; `tokens` is where an `oauth:` server's bearer comes
/// from (the roster's store, as for [`mcp_reconnect`]).
#[allow(clippy::too_many_arguments)]
pub async fn mcp_add(
    name: String,
    mut cfg: McpServerConfig,
    registry: &SharedRegistry,
    active: &ActiveServers,
    configs: &ServerConfigs,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
    tokens: Arc<dyn TokenStore>,
) -> Result<Vec<String>> {
    if cfg.disabled {
        bail!("cannot live-add a disabled MCP server `{name}` — omit `disabled` or set it false");
    }
    if let Some(existing) = configs
        .lock()
        .expect("MCP server-config mutex poisoned")
        .get(&name)
        .cloned()
    {
        carry_forward_unspecified_fields(&mut cfg, &existing, &name);
    }
    let (client, defs) = connect_client(&name, &cfg, secret_env, http, Some(tokens)).await?;
    let tools = install_server(registry, active, &name, client, defs, &cfg);
    configs
        .lock()
        .expect("MCP server-config mutex poisoned")
        .insert(name.clone(), cfg);
    tracing::info!("MCP server `{name}`: live-added, {} tool(s)", tools.len());
    Ok(tools)
}

/// Carry `headers`/`env` forward from `existing` into `cfg` when `cfg` left
/// them empty (#561), warning so the silent recovery is at least diagnosable.
/// Any other field the caller *did* specify (`url`, `command`, `args`,
/// `disabled`) always wins outright — only the two maps a bare re-add has no
/// way to express "unspecified" get this treatment.
fn carry_forward_unspecified_fields(
    cfg: &mut McpServerConfig,
    existing: &McpServerConfig,
    name: &str,
) {
    if cfg.headers.is_empty() && !existing.headers.is_empty() {
        tracing::warn!(
            "MCP server `{name}`: re-added with no --header flags — keeping the \
             existing {} header(s) instead of clobbering them",
            existing.headers.len()
        );
        cfg.headers = existing.headers.clone();
    }
    if cfg.env.is_empty() && !existing.env.is_empty() {
        tracing::warn!(
            "MCP server `{name}`: re-added with no env overrides — keeping the \
             existing {} env var(s) instead of clobbering them",
            existing.env.len()
        );
        cfg.env = existing.env.clone();
    }
}

/// Connect (or reconnect) a server **without touching `configs`** — the
/// OAuth counterpart to [`mcp_add`] (ADR-0153).
///
/// `/mcp connect` authorizes a server that is *already configured*, so no
/// `McpChanged` follows and the head's config is not rewritten (that would
/// round-trip the user's hand-authored `mcp:` block and drop its comments).
/// Otherwise identical to `mcp_add`'s upsert — old tools unregistered, old
/// connection dropped, new tools registered — with the awaits kept outside
/// every lock (#372). `http` (#559) is the shared endpoint pool the connect
/// rides; `tokens` holds the credential just obtained.
pub async fn mcp_reconnect(
    name: &str,
    cfg: &McpServerConfig,
    registry: &SharedRegistry,
    active: &ActiveServers,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
    tokens: Arc<dyn TokenStore>,
) -> Result<Vec<String>> {
    let (client, defs) = connect_client(name, cfg, secret_env, http, Some(tokens)).await?;
    let tools = install_server(registry, active, name, client, defs, cfg);
    tracing::info!("MCP server `{name}`: reconnected, {} tool(s)", tools.len());
    Ok(tools)
}

/// Drop a server's live connection and tools **without touching `configs`**
/// (ADR-0153) — what `/mcp disconnect` needs after revoking a credential: the
/// server stays configured, it simply has no usable session any more. A name
/// that was never connected is a no-op.
pub fn mcp_disconnect_only(
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
) -> Result<()> {
    drop_server(registry, active, name);
    Ok(())
}

/// Disconnect a server: unregister the tools it registered, drop it from
/// `active` (releasing the last `Arc<McpClient>` kills the subprocess/closes
/// the HTTP session), and from `configs`. Errors only if `name` is not in
/// `configs` at all — removing a name that is configured but never connected
/// (e.g. `disabled`, or failed at startup) still succeeds, dropping its
/// leftover config entry.
pub fn mcp_remove(
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
    configs: &ServerConfigs,
) -> Result<()> {
    drop_server(registry, active, name);
    let removed = configs
        .lock()
        .expect("MCP server-config mutex poisoned")
        .remove(name);
    if removed.is_none() {
        bail!("no MCP server named `{name}` in the configuration");
    }
    tracing::info!("MCP server `{name}`: removed live");
    Ok(())
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
            url: None,
            ..http_cfg("unused", HashMap::new())
        }
    }

    fn http_cfg(url: &str, headers: HashMap<String, String>) -> McpServerConfig {
        McpServerConfig {
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some(url.to_string()),
            headers,
            disabled: false,
            capabilities: HashMap::new(),
            oauth: None,
            description: None,
        }
    }

    // #561: re-adding without `--header` must not clobber a stored bearer.
    #[test]
    fn carry_forward_keeps_existing_headers_when_new_spec_is_headerless() {
        let existing = http_cfg(
            "https://example.com/mcp",
            HashMap::from([("Authorization".to_string(), "Bearer ${TOKEN}".to_string())]),
        );
        let mut cfg = http_cfg("https://example.com/mcp", HashMap::new());
        carry_forward_unspecified_fields(&mut cfg, &existing, "srv");
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer ${TOKEN}")
        );
    }

    // An explicit new header set always wins outright, never merges.
    #[test]
    fn carry_forward_leaves_an_explicit_new_header_set_untouched() {
        let existing = http_cfg(
            "https://example.com/mcp",
            HashMap::from([("Authorization".to_string(), "Bearer old".to_string())]),
        );
        let mut cfg = http_cfg(
            "https://example.com/mcp",
            HashMap::from([("Authorization".to_string(), "Bearer new".to_string())]),
        );
        carry_forward_unspecified_fields(&mut cfg, &existing, "srv");
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer new")
        );
    }

    fn no_tokens() -> Arc<dyn TokenStore> {
        Arc::new(crate::mcp::InMemoryTokenStore::default())
    }

    #[tokio::test]
    async fn mcp_add_refuses_a_disabled_server() {
        let registry = shared_registry();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));
        let mut cfg = stdio_cfg("definitely-not-a-real-binary-xyz");
        cfg.disabled = true;

        let http = crate::test_http_client();
        let err = mcp_add(
            "srv".into(),
            cfg,
            &registry,
            &active,
            &configs,
            &[],
            &http,
            no_tokens(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("disabled"));
        assert!(active.lock().unwrap().is_empty());
        assert!(configs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mcp_add_surfaces_a_connect_failure_without_touching_state() {
        let registry = shared_registry();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));

        let http = crate::test_http_client();
        let result = mcp_add(
            "broken".into(),
            stdio_cfg("definitely-not-a-real-binary-xyz"),
            &registry,
            &active,
            &configs,
            &[],
            &http,
            no_tokens(),
        )
        .await;
        assert!(result.is_err());
        assert!(active.lock().unwrap().is_empty());
        assert!(configs.lock().unwrap().is_empty());
        assert!(registry.read().unwrap().is_empty());
    }

    #[test]
    fn mcp_remove_errors_on_an_unknown_server() {
        let registry = shared_registry();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));

        let err = mcp_remove("nope", &registry, &active, &configs).unwrap_err();
        assert!(err.to_string().contains("no MCP server named"));
    }
}
