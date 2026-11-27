//! Tests for [`super`] (`mcp::available`, ADR-0226). Sibling file (`#[path]`
//! child module, so private fields stay reachable) to keep both sides of the
//! 400-line file cap.

use super::*;

use crate::mcp::active::{ActiveServer, ActiveServers};
use crate::mcp::McpClient;
use crate::tools::SharedRegistry;

fn http_entry(url: &str) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        disabled: false,
        capabilities: HashMap::new(),
        oauth: None,
        description: None,
    }
}

/// A stdio server config, for sibling test modules too.
pub(crate) fn stdio(command: &str) -> McpServerConfig {
    McpServerConfig {
        command: Some(command.to_string()),
        url: None,
        ..http_entry("unused")
    }
}

fn registry() -> SharedRegistry {
    Arc::new(RwLock::new(crate::tools::ToolRegistry::new()))
}

fn active() -> ActiveServers {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Registered = available: every `mcp:` entry is listed unless
/// `disabled: true`, nothing connects, and a live add/remove updates the
/// roster.
#[test]
fn user_entries_are_available_unless_disabled_and_live_edits_apply() {
    let user = HashMap::from([
        ("kb".to_string(), http_entry("https://kb/mcp")),
        (
            "off".to_string(),
            McpServerConfig {
                disabled: true,
                ..http_entry("https://off/mcp")
            },
        ),
    ]);
    let avail = AvailableMcp::from_config(&user, vec![]);
    assert_eq!(avail.available_names(), vec!["kb"]);
    assert_eq!(avail.tier_of("off"), McpTier::Disabled);
    assert!(avail.get("off").is_none());
    assert_eq!(avail.tier_of("nope"), McpTier::Unknown);
    avail.insert_user("gh", http_entry("https://gh/mcp"));
    assert_eq!(avail.available_names(), vec!["gh", "kb"]);
    avail.remove("kb");
    assert_eq!(avail.available_names(), vec!["gh"]);
}

/// Connecting an unknown server errors and names the available set.
#[tokio::test]
async fn connecting_an_unknown_server_errors() {
    let avail = AvailableMcp::default();
    avail.insert_user("kb", http_entry("https://kb/mcp"));
    let http = crate::test_http_client();
    let err = ensure_connected(&avail, "nope", &registry(), &active(), &http)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("no available MCP server named `nope`"),
        "{msg}"
    );
    assert!(msg.contains("kb"), "{msg}");
}

/// An already-connected server answers from `active` — no reconnect, even if
/// the roster no longer lists it.
#[tokio::test]
async fn an_already_connected_server_is_not_reconnected() {
    let avail = AvailableMcp::default();
    let active = active();
    active.lock().unwrap().insert(
        "kb".to_string(),
        ActiveServer {
            client: Arc::new(fake_client()),
            tools: vec!["mcp__kb__search".into()],
            transport: "stdio".into(),
        },
    );
    let http = crate::test_http_client();
    let tools = ensure_connected(&avail, "kb", &registry(), &active, &http)
        .await
        .unwrap();
    assert_eq!(tools, vec!["mcp__kb__search".to_string()]);
}

/// The per-server connect guard (#556) is the same lock for repeated lookups
/// of the same name, and a distinct one per name.
#[test]
fn connect_guard_is_shared_per_server_name_only() {
    let avail = AvailableMcp::default();
    let a1 = avail.connect_guard("a");
    let a2 = avail.connect_guard("a");
    let b = avail.connect_guard("b");
    assert!(Arc::ptr_eq(&a1, &a2));
    assert!(!Arc::ptr_eq(&a1, &b));
}

/// Two concurrent first connects of the same unreachable server neither
/// panic nor deadlock (#556) and leave no partial state behind.
#[tokio::test]
async fn concurrent_connects_of_the_same_server_do_not_corrupt_state() {
    let avail = Arc::new(AvailableMcp::default());
    // Port 1 on loopback refuses outright, so the connect fails in ms — a
    // black-holed address would ride the full retry/timeout ladder instead.
    avail.insert_user("remote", http_entry("http://127.0.0.1:1/mcp"));
    let (reg, act) = (registry(), active());
    let http = crate::test_http_client();
    let run = |avail: Arc<AvailableMcp>| {
        let (reg, act, http) = (reg.clone(), act.clone(), http.clone());
        async move { ensure_connected(&avail, "remote", &reg, &act, &http).await }
    };
    let (a, b) = tokio::join!(run(avail.clone()), run(avail.clone()));
    assert!(a.is_err() && b.is_err());
    assert!(reg.read().unwrap().is_empty());
    assert!(act.lock().unwrap().is_empty());
}

// A minimal `McpClient` for tests that never call it (mirrors `live.rs`).
pub(crate) fn fake_client() -> McpClient {
    let (a, _b) = tokio::io::duplex(64);
    let (reader, writer) = tokio::io::split(a);
    McpClient::Stdio(crate::mcp::StdioClient::new(
        "fake".to_string(),
        writer,
        reader,
        None,
    ))
}

/// Two names whose registered tool names overlap — identical after
/// sanitizing, or one nesting the other's prefix — would make every tool
/// name and `mcp__<s>__*` pattern ambiguous: only the first (by name) is
/// registered, and a live add of a clashing name is detectable.
#[test]
fn clashing_server_names_keep_only_the_first() {
    let user = HashMap::from([
        ("my.docs".to_string(), http_entry("https://a/mcp")),
        ("my_docs".to_string(), http_entry("https://b/mcp")),
        ("a".to_string(), http_entry("https://c/mcp")),
        ("a__b".to_string(), http_entry("https://d/mcp")),
        ("gh".to_string(), http_entry("https://e/mcp")),
    ]);
    let avail = AvailableMcp::from_config(&user, vec![]);
    assert_eq!(avail.available_names(), vec!["a", "gh", "my.docs"]);
    let warnings = avail.clash_warnings();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings
        .iter()
        .any(|w| w.contains("`my_docs` ignored") && w.contains("`my.docs`")));
    assert!(warnings.iter().any(|w| w.contains("`a__b` ignored")));
    assert_eq!(avail.clashing_name("my_docs").as_deref(), Some("my.docs"));
    assert_eq!(avail.clashing_name("a__b").as_deref(), Some("a"));
    assert_eq!(avail.clashing_name("kb"), None);
    assert_eq!(
        avail.clashing_name("gh"),
        None,
        "a name never clashes with itself"
    );
}

/// A disabled server claims no name: a clashing server that can connect
/// stays.
#[test]
fn a_disabled_server_never_knocks_out_a_clashing_one() {
    let off = McpServerConfig {
        disabled: true,
        ..http_entry("https://off/mcp")
    };
    let user = HashMap::from([
        ("a".to_string(), off),
        ("a__b".to_string(), http_entry("https://ab/mcp")),
    ]);
    let avail = AvailableMcp::from_config(&user, vec![]);
    assert_eq!(avail.available_names(), vec!["a__b"]);
    assert!(
        avail.clash_warnings().is_empty(),
        "{:?}",
        avail.clash_warnings()
    );
}

#[test]
fn a_disabled_server_clashing_with_an_active_one_never_steals_its_routing() {
    let off = McpServerConfig {
        disabled: true,
        ..http_entry("https://off/mcp")
    };
    let user = HashMap::from([
        ("a".to_string(), http_entry("https://a/mcp")),
        ("a__b".to_string(), off),
    ]);
    let avail = AvailableMcp::from_config(&user, vec![]);
    assert_eq!(avail.available_names(), vec!["a"]);
    assert_eq!(
        avail.server_for_tool("mcp__a__b__x").as_deref(),
        Some("a"),
        "the active claimant keeps routing, not the disabled clasher"
    );
    assert_eq!(avail.tier_of("a__b"), McpTier::Unknown);
    assert_eq!(avail.tier_of("a"), McpTier::Available);
}
