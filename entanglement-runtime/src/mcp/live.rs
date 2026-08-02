//! Live MCP server management (#375, Phase 4 of the MCP umbrella): hot-add/
//! remove a server in the running process, mirrored to `config.yml` so it
//! survives a restart.
//!
//! `ActiveServers` tracks exactly what is currently connected — seeded at
//! startup from [`super::connect`]'s return, updated by [`mcp_add`]/
//! [`mcp_remove`]. `ServerConfigs` is the separate, wider live mirror of every
//! *configured* server (including one that failed to connect or is
//! `disabled`) — the full set [`crate::config::save_mcp`] must write back,
//! since it replaces the whole `mcp:` section. Losing track of a
//! never-connected entry there would silently drop it from `config.yml` on
//! the next live add/remove.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};

use crate::tools::SharedRegistry;

use super::{connect_client, register_tools, transport_label, McpClient, McpServerConfig};

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

/// The live mirror of every *configured* server — wider than [`ActiveServers`]
/// (includes `disabled`/failed-to-connect entries) — so a `save_mcp` write
/// after one add/remove never drops an unrelated entry. Seeded at startup from
/// the user config's `mcp:` section.
pub type ServerConfigs = Arc<Mutex<HashMap<String, McpServerConfig>>>;

/// Hot-connect a server: `connect` → register its tools into `registry` →
/// track it in `active` → persist the full server set to `config.yml`.
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
/// the connect rides — a live `/mcp add` is always a user-declared server, so
/// it carries no provider key (unkeyed, still pooled) bucket.
#[allow(clippy::too_many_arguments)]
pub async fn mcp_add(
    name: String,
    mut cfg: McpServerConfig,
    registry: &SharedRegistry,
    active: &ActiveServers,
    configs: &ServerConfigs,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
) -> Result<Vec<String>> {
    if cfg.disabled {
        bail!("cannot live-add a disabled MCP server `{name}` — omit `disabled` or set it false");
    }
    if let Some(existing) = configs.lock().unwrap().get(&name).cloned() {
        carry_forward_unspecified_fields(&mut cfg, &existing, &name);
    }
    let (client, defs) = connect_client(&name, &cfg, secret_env, http, None).await?;
    let prefix = format!("mcp__{name}__");
    let tools = {
        let mut reg = registry.write().unwrap();
        reg.unregister_prefix(&prefix);
        register_tools(&mut reg, &client, &name, defs)
    };
    let transport = transport_label(&cfg);
    active.lock().unwrap().insert(
        name.clone(),
        ActiveServer {
            client,
            tools: tools.clone(),
            transport,
        },
    );
    {
        let mut all = configs.lock().unwrap();
        all.insert(name.clone(), cfg);
        crate::config::save_mcp(&all)?;
    }
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

/// Connect (or reconnect) a server **without touching `config.yml`** — the
/// OAuth counterpart to [`mcp_add`] (ADR-0153).
///
/// `/mcp connect` authorizes a server that is *already configured*, so the
/// config must not be rewritten: doing so would round-trip the user's hand-
/// authored `mcp:` block through serde and drop its comments for no reason.
/// Otherwise identical to `mcp_add`'s upsert — old tools unregistered, old
/// connection dropped, new tools registered — with the awaits kept outside
/// every lock (#372). `http` (#559) is the shared endpoint pool the connect
/// rides; an OAuth-reconnected server authenticates via its bearer token, not
/// a shared provider key, so it carries no `api_key` (unkeyed, still pooled).
pub async fn mcp_reconnect(
    name: &str,
    cfg: &McpServerConfig,
    registry: &SharedRegistry,
    active: &ActiveServers,
    secret_env: &[String],
    http: &entanglement_core::HttpClient,
) -> Result<Vec<String>> {
    let (client, defs) = connect_client(name, cfg, secret_env, http, None).await?;
    let prefix = format!("mcp__{name}__");
    let tools = {
        let mut reg = registry.write().unwrap();
        reg.unregister_prefix(&prefix);
        register_tools(&mut reg, &client, name, defs)
    };
    let transport = transport_label(cfg);
    active.lock().unwrap().insert(
        name.to_string(),
        ActiveServer {
            client,
            tools: tools.clone(),
            transport,
        },
    );
    tracing::info!("MCP server `{name}`: reconnected, {} tool(s)", tools.len());
    Ok(tools)
}

/// Drop a server's live connection and tools **without touching `config.yml`**
/// (ADR-0153) — what `/mcp disconnect` needs after revoking a credential: the
/// server stays configured, it simply has no usable session any more. A name
/// that was never connected is a no-op.
pub fn mcp_disconnect_only(
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
) -> Result<()> {
    registry
        .write()
        .unwrap()
        .unregister_prefix(&format!("mcp__{name}__"));
    active.lock().unwrap().remove(name);
    Ok(())
}

/// Disconnect a server: unregister its `mcp__{name}__*` tools, drop it from
/// `active` (releasing the last `Arc<McpClient>` kills the subprocess/closes
/// the HTTP session), and persist the removal. Errors only if `name` is not in
/// `configs` at all — removing a name that is configured but never connected
/// (e.g. `disabled`, or failed at startup) still succeeds, dropping its
/// leftover config entry.
pub fn mcp_remove(
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
    configs: &ServerConfigs,
) -> Result<()> {
    registry
        .write()
        .unwrap()
        .unregister_prefix(&format!("mcp__{name}__"));
    active.lock().unwrap().remove(name);
    let mut all = configs.lock().unwrap();
    if all.remove(name).is_none() {
        bail!("no MCP server named `{name}` in the configuration");
    }
    crate::config::save_mcp(&all)?;
    tracing::info!("MCP server `{name}`: removed live");
    Ok(())
}

/// Enumerate every currently-connected server, sorted by name for stable
/// output.
pub fn mcp_list(active: &ActiveServers) -> Vec<entanglement_core::McpServerStatus> {
    let active = active.lock().unwrap();
    let mut list: Vec<entanglement_core::McpServerStatus> = active
        .iter()
        .map(|(name, server)| entanglement_core::McpServerStatus {
            name: name.clone(),
            transport: server.transport.clone(),
            connected: true,
            tools: server.tools.clone(),
            error: None,
            // Filled by the MCP responder (#542), which alone knows whether a
            // connected server is startup-`enabled` or lazily `allowed`.
            state: None,
            // Likewise filled by the responder, which holds the token store.
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
            state: None,
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
            state: None,
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

    #[tokio::test]
    async fn mcp_add_refuses_a_disabled_server() {
        let registry = shared_registry();
        let active: ActiveServers = Arc::new(Mutex::new(HashMap::new()));
        let configs: ServerConfigs = Arc::new(Mutex::new(HashMap::new()));
        let mut cfg = stdio_cfg("definitely-not-a-real-binary-xyz");
        cfg.disabled = true;

        let http = entanglement_core::HttpClient::default();
        let err = mcp_add("srv".into(), cfg, &registry, &active, &configs, &[], &http)
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

        let http = entanglement_core::HttpClient::default();
        let result = mcp_add(
            "broken".into(),
            stdio_cfg("definitely-not-a-real-binary-xyz"),
            &registry,
            &active,
            &configs,
            &[],
            &http,
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
