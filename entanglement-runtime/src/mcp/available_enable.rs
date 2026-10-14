//! Lazy connect for [`super::AvailableMcp`] (ADR-0226): the first
//! `mcp_tools` read or `mcp(...)` dispatch on a server connects it for the
//! whole process. Split out of `available.rs` along the 400-line file cap
//! (#556) — a sibling `#[path]` child module, so the roster's private fields
//! stay reachable via the normal descendant-module visibility rule.

use std::time::Duration;

use anyhow::{bail, Result};

use crate::tools::SharedRegistry;

use super::super::live::{drop_server, install_server, ActiveServers};
use super::super::{connect_client, record_last_seen};
use super::AvailableMcp;

/// Ceiling on a lazy connect (#556): a hung server must not park the calling
/// turn — nor every other caller waiting on the per-server
/// [`AvailableMcp::connect_guard`] — forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Connect an available server if it isn't already, registering its tools
/// into the live `registry` (no persistence — `ServerConfigs` and
/// `config.yml` are never touched), and record its tool names in the
/// last-seen cache. Returns the server's registered tool names. `http`
/// (#559) is the shared endpoint pool the connect rides; a bundled server's
/// `key_env` is resolved live so its traffic shares pool-key identity with
/// the LLM endpoint billed against the same key.
pub async fn ensure_connected(
    avail: &AvailableMcp,
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
    http: &entanglement_core::HttpClient,
) -> Result<Vec<String>> {
    connect_guarded(avail, name, registry, active, http, false).await
}

/// [`ensure_connected`]'s guarded section. With `cooldown`, the
/// [`FAILURE_COOLDOWN`] guard is consulted and recorded *under* the
/// per-server guard (#831): a caller that queued behind a failing connect
/// returns its recorded failure instead of reconnecting.
async fn connect_guarded(
    avail: &AvailableMcp,
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
    http: &entanglement_core::HttpClient,
    cooldown: bool,
) -> Result<Vec<String>> {
    if let Some(tools) = connected_tools(active, name) {
        return Ok(tools);
    }
    // Serialize the check-then-connect-then-register sequence per server
    // (#556 TOCTOU), then re-check: another caller may have finished
    // connecting while this one waited.
    let guard = avail.connect_guard(name);
    let _permit = guard.lock().await;
    if let Some(tools) = connected_tools(active, name) {
        return Ok(tools);
    }
    if cooldown && avail.recently_failed_connect(name, FAILURE_COOLDOWN) {
        bail!(COOLDOWN_CAUSE);
    }
    let connected = connect_locked(avail, name, registry, active, http).await;
    if cooldown {
        match &connected {
            Ok(_) => avail.clear_connect_failure(name),
            Err(_) => avail.record_connect_failure(name),
        }
    }
    connected
}

/// The connect itself; the caller holds `name`'s connect guard.
async fn connect_locked(
    avail: &AvailableMcp,
    name: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
    http: &entanglement_core::HttpClient,
) -> Result<Vec<String>> {
    let Some(server) = avail.get(name) else {
        bail!(
            "no available MCP server named `{name}` (available: {})",
            avail.available_names().join(", ")
        );
    };
    if avail.needs_auth(name, &server.config) {
        bail!(auth_hint(name));
    }
    let api_key = server
        .key_env
        .as_deref()
        .and_then(|var| std::env::var(var).ok());
    let connect = connect_client(
        name,
        &server.config,
        avail.secret_env(),
        http,
        api_key.as_deref(),
        None,
    );
    let (client, defs) = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(result) => result?,
        Err(_) => bail!("connecting MCP server `{name}` timed out"),
    };
    let tools = install_server(registry, active, name, client, defs, &server.config);
    record_last_seen(avail.tool_cache(), name, &server.config, &tools).await;
    tracing::info!("MCP server `{name}`: connected, {} tool(s)", tools.len());
    Ok(tools)
}

fn auth_hint(name: &str) -> String {
    format!("MCP server `{name}` needs authorization — the user can run `/mcp connect {name}`")
}

fn connected_tools(active: &ActiveServers, name: &str) -> Option<Vec<String>> {
    active
        .lock()
        .expect("MCP active-server mutex poisoned")
        .get(name)
        .map(|s| s.tools.clone())
}

/// Cooldown window for [`try_lazy_connect`]'s stampede guard (ADR-0201): a
/// dispatch-time connect failure within this window short-circuits a later
/// unknown `mcp__<server>__*` call against the same server to the same error
/// instead of each retrying the full [`CONNECT_TIMEOUT`] connect.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// Why a connect inside the [`FAILURE_COOLDOWN`] window was not attempted.
const COOLDOWN_CAUSE: &str = "recent connect failure, retry shortly";

/// How a dispatch-time "unknown tool" resolution for an `mcp__<server>__*`
/// name settled (ADR-0201) — see [`try_lazy_connect`].
pub enum LazyConnectOutcome {
    /// [`super::McpTier::Unknown`]: no server under this name — the caller
    /// falls through to the ordinary unknown-tool message.
    Unknown,
    /// [`super::McpTier::Disabled`]: known but configured off — a truthful,
    /// attributed decline ([`super::disabled_decline`]).
    Disabled,
    /// Connected (now or earlier). The caller must re-fetch its registry
    /// snapshot (this registered into the *live* registry, invisible to an
    /// already-cloned one) and continue the dispatch ladder — the call is
    /// still graded like any other.
    Connected,
    /// The connect failed — fresh, or short-circuited by the
    /// [`FAILURE_COOLDOWN`] guard with the same message.
    Failed(String),
}

/// Resolve a dispatch-time "unknown tool" for an `mcp__<server>__*` name whose
/// server isn't connected yet (ADR-0201's dispatch-time connect, kept by
/// ADR-0226 for a direct `mcp(...)` on a server never read with `mcp_tools`).
/// `http: None` (no HTTP client wired into the caller) is a connect failure,
/// never a panic — every in-tree head wires one.
pub async fn try_lazy_connect(
    avail: &AvailableMcp,
    server: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
    http: Option<&entanglement_core::HttpClient>,
) -> LazyConnectOutcome {
    match avail.tier_of(server) {
        super::McpTier::Unknown => return LazyConnectOutcome::Unknown,
        super::McpTier::Disabled => return LazyConnectOutcome::Disabled,
        super::McpTier::Available => {}
    }
    // Connected wins over a stale cooldown: a `/mcp connect` may have
    // succeeded since the last failure.
    if connected_tools(active, server).is_some() {
        return LazyConnectOutcome::Connected;
    }
    // Missing credentials are a user action away, not a transport failure:
    // never start a cooldown that would outlive a successful `/mcp connect`.
    if let Some(server_cfg) = avail.get(server) {
        if avail.needs_auth(server, &server_cfg.config) {
            return LazyConnectOutcome::Failed(auth_hint(server));
        }
    }
    if avail.recently_failed_connect(server, FAILURE_COOLDOWN) {
        return LazyConnectOutcome::Failed(format!(
            "mcp server `{server}` could not be connected: {COOLDOWN_CAUSE}"
        ));
    }
    let Some(http) = http else {
        return LazyConnectOutcome::Failed(format!(
            "mcp server `{server}` could not be connected: no HTTP client configured for lazy \
             MCP connects"
        ));
    };
    match connect_guarded(avail, server, registry, active, http, true).await {
        Ok(_tools) => LazyConnectOutcome::Connected,
        Err(e) => LazyConnectOutcome::Failed(format!(
            "mcp server `{server}` could not be connected: {e:#}"
        )),
    }
}

/// Resolve a dispatch for an unregistered `mcp__*` name (ADR-0201, kept by
/// ADR-0226): find the owning server by its registered prefix, connect it
/// lazily, and hand back a fresh registry snapshot holding the tool — the
/// caller's own snapshot predates the connect. `Err` is the decline to reply.
pub async fn resolve_unregistered(
    avail: &AvailableMcp,
    tool: &str,
    registry: &SharedRegistry,
    active: &ActiveServers,
    http: Option<&entanglement_core::HttpClient>,
) -> Result<crate::tools::ToolRegistry, String> {
    let Some(server) = avail.server_for_tool(tool) else {
        let shown = super::server_name_of(tool).unwrap_or(tool);
        return Err(super::unknown_server_decline(shown));
    };
    match try_lazy_connect(avail, &server, registry, active, http).await {
        LazyConnectOutcome::Connected => {}
        LazyConnectOutcome::Unknown => return Err(super::unknown_server_decline(&server)),
        LazyConnectOutcome::Disabled => return Err(super::disabled_decline(&server)),
        LazyConnectOutcome::Failed(msg) => return Err(msg),
    }
    let snapshot = registry
        .read()
        .expect("tool registry lock poisoned")
        .clone();
    if snapshot.contains(tool) {
        Ok(snapshot)
    } else {
        Err(super::missing_tool_decline(&server, tool))
    }
}

/// Drop a server's connection and tools (the `/mcp remove` arm for a bundled
/// name). Nothing is persisted — the server stays available and the next
/// `mcp_tools`/`mcp(...)` reconnects it.
pub fn disconnect(name: &str, registry: &SharedRegistry, active: &ActiveServers) {
    drop_server(registry, active, name);
    tracing::info!("MCP server `{name}`: disconnected (still available)");
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::mcp::live::ActiveServer;
    use std::sync::{Arc, Mutex, RwLock};

    use super::*;

    fn registry() -> SharedRegistry {
        Arc::new(RwLock::new(crate::tools::ToolRegistry::new()))
    }
    fn active() -> ActiveServers {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn unknown_and_disabled_servers_short_circuit_without_a_connect() {
        let mut avail = AvailableMcp::default();
        avail.disabled_names.insert("off".to_string());
        let unknown = try_lazy_connect(&avail, "nope", &registry(), &active(), None).await;
        assert!(matches!(unknown, LazyConnectOutcome::Unknown));
        let off = try_lazy_connect(&avail, "off", &registry(), &active(), None).await;
        assert!(matches!(off, LazyConnectOutcome::Disabled));
    }

    #[tokio::test]
    async fn an_available_server_with_no_http_client_fails_cleanly() {
        let avail = AvailableMcp::default();
        avail.insert_user("lazy", crate::mcp::available::tests::stdio("true"));
        match try_lazy_connect(&avail, "lazy", &registry(), &active(), None).await {
            LazyConnectOutcome::Failed(msg) => assert!(msg.contains("no HTTP client"), "{msg}"),
            _ => panic!("expected a Failed outcome"),
        }
    }

    /// S2 / #818 review: `mcp_tools` on an OAuth server with no credential
    /// answers the auth hint without starting a cooldown, and once `/mcp
    /// connect` has connected it (simulated: the server lands in `active`,
    /// as `mcp_reconnect` does) the very next read works — even past a
    /// recorded failure.
    #[tokio::test]
    async fn auth_required_starts_no_cooldown_and_a_later_connect_is_used_at_once() {
        let avail =
            AvailableMcp::default().with_token_store(crate::config::McpTokenStore::at(None));
        let mut cfg = crate::mcp::available::tests::stdio("true");
        cfg.oauth = Some(Default::default());
        avail.insert_user("kb", cfg);
        let (reg, act) = (registry(), active());
        match try_lazy_connect(&avail, "kb", &reg, &act, None).await {
            LazyConnectOutcome::Failed(msg) => assert!(msg.contains("/mcp connect kb"), "{msg}"),
            _ => panic!("expected the auth hint"),
        }
        assert!(!avail.recently_failed_connect("kb", FAILURE_COOLDOWN));
        avail.record_connect_failure("kb");
        act.lock().unwrap().insert(
            "kb".to_string(),
            ActiveServer {
                client: Arc::new(crate::mcp::available::tests::fake_client()),
                tools: vec!["mcp__kb__search".into()],
                transport: "stdio".into(),
            },
        );
        assert!(matches!(
            try_lazy_connect(&avail, "kb", &reg, &act, None).await,
            LazyConnectOutcome::Connected
        ));
    }

    #[tokio::test]
    async fn an_unspawnable_server_fails_and_registers_nothing() {
        let avail = AvailableMcp::default();
        avail.insert_user(
            "broken",
            crate::mcp::available::tests::stdio("definitely-not-a-real-binary-xyz"),
        );
        let (reg, act) = (registry(), active());
        let http = crate::test_http_client();
        assert!(ensure_connected(&avail, "broken", &reg, &act, &http)
            .await
            .is_err());
        assert!(reg.read().unwrap().is_empty());
        assert!(act.lock().unwrap().is_empty());
    }

    /// #831: callers queued behind a failing connect return its recorded
    /// failure instead of each reconnecting in turn.
    #[tokio::test]
    async fn waiters_queued_behind_a_failing_connect_do_not_reconnect() {
        let dir = std::env::temp_dir().join(format!("mcp-queued-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spawns = dir.join("spawns");
        let mut cfg = crate::mcp::available::tests::stdio("sh");
        cfg.args = vec![
            "-c".into(),
            format!("echo x >> '{}'; sleep 0.3", spawns.display()),
        ];
        let avail = Arc::new(AvailableMcp::default());
        avail.insert_user("flaky", cfg);
        let (reg, act) = (registry(), active());
        let http = crate::test_http_client();
        let calls = (0..3).map(|_| {
            let (avail, reg, act, http) = (avail.clone(), reg.clone(), act.clone(), http.clone());
            tokio::spawn(
                async move { try_lazy_connect(&avail, "flaky", &reg, &act, Some(&http)).await },
            )
        });
        for call in calls.collect::<Vec<_>>() {
            assert!(matches!(call.await.unwrap(), LazyConnectOutcome::Failed(_)));
        }
        let count = std::fs::read_to_string(&spawns).unwrap().lines().count();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(count, 1, "one connect attempt");
    }
}
