//! Tests for [`super`] (`mcp::available`, #542). Sibling file (`#[path]`
//! child module, so private fields stay reachable) to keep both sides of the
//! 400-line file cap.

use super::*;

use crate::mcp::McpClient;

fn user_entry(url: &str, state: Option<McpServerState>) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        disabled: false,
        capabilities: HashMap::new(),
        state,
    }
}

/// The embedded z.ai bundle + a keyed env: `allowed` by default, so nothing
/// joins the startup set and all three are available.
#[test]
fn partition_bundles_default_to_allowed() {
    // Repoint the bundle's key env at a test-unique var so this never races
    // another test (or the developer's real ZAI_API_KEY) via process env.
    let mut catalog = Catalog::builtin();
    for p in &mut catalog.providers {
        if p.name == "zai" {
            p.key_env = Some("ZAI_TEST_KEY_542_PARTITION".into());
        }
    }
    // The key env is read live per check, not at partition time.
    std::env::set_var("ZAI_TEST_KEY_542_PARTITION", "test-key");
    let (startup, avail) = AvailableMcp::partition(&catalog, &HashMap::new(), vec![]);
    assert!(startup.is_empty());
    assert_eq!(
        avail.available_names(),
        vec!["web_reader", "web_search_prime", "zread"]
    );
    let zread = avail.get("zread").unwrap();
    assert_eq!(zread.provider.as_deref(), Some("zai"));
    assert_eq!(zread.key_env.as_deref(), Some("ZAI_TEST_KEY_542_PARTITION"));
    std::env::remove_var("ZAI_TEST_KEY_542_PARTITION");
}

/// Keyless ⇒ silently absent: not listed, not gettable.
#[test]
fn keyless_bundle_is_silently_absent() {
    let mut catalog = Catalog::builtin();
    // Point the bundle at an env var that is definitely unset.
    for p in &mut catalog.providers {
        if p.name == "zai" {
            p.key_env = Some("DEFINITELY_UNSET_TEST_KEY_542".into());
        }
    }
    let (startup, avail) = AvailableMcp::partition(&catalog, &HashMap::new(), vec![]);
    assert!(startup.is_empty());
    assert!(avail.available_names().is_empty());
    assert!(avail.get("zread").is_none());
}

/// A user `mcp:` override merges field-wise over the bundled definition:
/// `state: disabled` alone kills one bundle; `state: enabled` promotes
/// another into the startup set keeping its bundled url.
#[test]
fn user_override_merges_over_bundle_by_name() {
    let catalog = Catalog::builtin();
    std::env::set_var("ZAI_API_KEY_OVR", "k");
    let mut catalog = catalog;
    for p in &mut catalog.providers {
        if p.name == "zai" {
            p.key_env = Some("ZAI_API_KEY_OVR".into());
        }
    }
    let mut user = HashMap::new();
    let mut disable = user_entry("ignored", Some(McpServerState::Disabled));
    disable.url = None; // state-only override
    user.insert("web_reader".to_string(), disable);
    let mut promote = user_entry("ignored", Some(McpServerState::Enabled));
    promote.url = None;
    user.insert("zread".to_string(), promote);
    let (startup, avail) = AvailableMcp::partition(&catalog, &user, vec![]);
    // zread promoted to startup, with the bundled url intact.
    assert!(startup
        .get("zread")
        .unwrap()
        .url
        .as_deref()
        .unwrap()
        .contains("zread"));
    // web_reader gone entirely; web_search_prime still available.
    assert_eq!(avail.available_names(), vec!["web_search_prime"]);
    std::env::remove_var("ZAI_API_KEY_OVR");
}

/// A user-declared (non-bundled) server with `state: allowed` joins the
/// available set ungated; `enabled`/legacy entries keep startup semantics.
#[test]
fn user_entries_partition_by_state() {
    let catalog = Catalog { providers: vec![] };
    let mut user = HashMap::new();
    user.insert(
        "mine".to_string(),
        user_entry("https://example.com/mcp", Some(McpServerState::Allowed)),
    );
    user.insert(
        "startup".to_string(),
        user_entry("https://example.com/mcp2", None),
    );
    let mut off = user_entry("https://example.com/mcp3", None);
    off.disabled = true;
    user.insert("legacy-off".to_string(), off);
    let (startup, avail) = AvailableMcp::partition(&catalog, &user, vec![]);
    assert_eq!(startup.len(), 1);
    assert!(startup.contains_key("startup"));
    assert_eq!(avail.available_names(), vec!["mine"]);
    assert!(
        avail.get("mine").unwrap().key_ok(),
        "user entries are ungated"
    );
}

/// Spec visibility (#542): tools of a lazily-connected server are visible only
/// to enabling sessions; host tools and unknown servers always pass.
#[test]
fn spec_visibility_is_scoped_to_enabling_sessions() {
    let avail = AvailableMcp::default();
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    // Not lazily connected yet: visible (startup servers behave like this).
    assert!(avail.spec_visible("mcp__zread__search_doc", &a));
    assert!(avail.spec_visible("read", &a));
    avail.mark_enabled("zread", &a);
    assert!(avail.spec_visible("mcp__zread__search_doc", &a));
    assert!(!avail.spec_visible("mcp__zread__search_doc", &b));
    assert!(avail.is_lazy("zread"));
    // Symmetric session-scoped disable hides it again.
    avail.mark_disabled("zread", &a);
    assert!(!avail.spec_visible("mcp__zread__search_doc", &a));
    // Full disconnect clears the lazy mark → globally visible again (it will
    // also have been unregistered, so nothing actually advertises).
    let registry: SharedRegistry =
        std::sync::Arc::new(std::sync::RwLock::new(crate::tools::ToolRegistry::new()));
    let active: ActiveServers = std::sync::Arc::new(Mutex::new(HashMap::new()));
    disconnect(&avail, "zread", &registry, &active);
    assert!(!avail.is_lazy("zread"));
    assert!(avail.spec_visible("mcp__zread__search_doc", &b));
}

/// Enabling an unknown/absent server errors and names the available set.
#[tokio::test]
async fn enable_unknown_server_errors() {
    let avail = AvailableMcp::default();
    let registry: SharedRegistry =
        std::sync::Arc::new(std::sync::RwLock::new(crate::tools::ToolRegistry::new()));
    let active: ActiveServers = std::sync::Arc::new(Mutex::new(HashMap::new()));
    let err = enable_for_session(&avail, "nope", &SessionId::new("s"), &registry, &active)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no available MCP server"), "{err}");
}

/// Enabling a startup-`enabled` server (connected, but not in the available
/// roster) is a no-op that returns its tools — it must NOT become lazy-scoped
/// (that would hide it from every other session).
#[tokio::test]
async fn enable_startup_server_never_scopes_it() {
    let avail = AvailableMcp::default();
    let registry: SharedRegistry =
        std::sync::Arc::new(std::sync::RwLock::new(crate::tools::ToolRegistry::new()));
    let active: ActiveServers = std::sync::Arc::new(Mutex::new(HashMap::new()));
    active.lock().unwrap().insert(
        "startup".to_string(),
        ActiveServer {
            client: std::sync::Arc::new(fake_client()),
            tools: vec!["mcp__startup__t".into()],
            transport: "http".into(),
        },
    );
    let s = SessionId::new("s");
    let tools = enable_for_session(&avail, "startup", &s, &registry, &active)
        .await
        .unwrap();
    assert_eq!(tools, vec!["mcp__startup__t".to_string()]);
    assert!(!avail.is_lazy("startup"));
    // Still globally visible.
    assert!(avail.spec_visible("mcp__startup__t", &SessionId::new("other")));
}

/// Enabling a server that is already connected (e.g. by another session) only
/// marks the session — no reconnect.
#[tokio::test]
async fn enable_already_connected_marks_only() {
    let mut servers = HashMap::new();
    servers.insert(
        "srv".to_string(),
        AvailableServer {
            config: user_entry("https://example.com/mcp", Some(McpServerState::Allowed)),
            key_env: None,
            provider: None,
        },
    );
    let avail = AvailableMcp {
        servers,
        enabled: Mutex::new(HashMap::new()),
        secret_env: vec![],
    };
    let registry: SharedRegistry =
        std::sync::Arc::new(std::sync::RwLock::new(crate::tools::ToolRegistry::new()));
    let active: ActiveServers = std::sync::Arc::new(Mutex::new(HashMap::new()));
    active.lock().unwrap().insert(
        "srv".to_string(),
        ActiveServer {
            client: std::sync::Arc::new(fake_client()),
            tools: vec!["mcp__srv__t".into()],
            transport: "http".into(),
        },
    );
    let s = SessionId::new("s");
    let tools = enable_for_session(&avail, "srv", &s, &registry, &active)
        .await
        .unwrap();
    assert_eq!(tools, vec!["mcp__srv__t".to_string()]);
    assert!(avail.spec_visible("mcp__srv__t", &s));
}

/// Transport-exclusive user override: setting `url` clears a bundled
/// `command` (and vice versa) so the XOR invariant holds after the merge.
#[test]
fn merge_keeps_transport_xor() {
    let mut bundled = user_entry("https://bundled.example/mcp", None);
    let mut user = user_entry("x", None);
    user.url = None;
    user.command = Some("local-proxy".into());
    let merged = merge_user_over_bundled(bundled.clone(), &user);
    assert_eq!(merged.command.as_deref(), Some("local-proxy"));
    assert!(merged.url.is_none());
    assert!(merged.transport().is_ok());
    // And url over command.
    bundled.url = None;
    bundled.command = Some("bundled-cmd".into());
    let mut user2 = user_entry("https://user.example/mcp", None);
    user2
        .headers
        .insert("Authorization".into(), "Bearer x".into());
    let merged = merge_user_over_bundled(bundled, &user2);
    assert!(merged.command.is_none());
    assert_eq!(merged.url.as_deref(), Some("https://user.example/mcp"));
    assert!(merged.transport().is_ok());
}

// A minimal `McpClient` for tests that never call it (mirrors `live.rs`).
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
