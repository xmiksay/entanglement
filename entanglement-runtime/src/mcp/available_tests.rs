//! Tests for [`super`] (`mcp::available`, ADR-0226). Sibling file (`#[path]`
//! child module, so private fields stay reachable) to keep both sides of the
//! 400-line file cap.

use super::*;

use crate::mcp::live::{ActiveServer, ActiveServers};
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

/// The embedded catalog with z.ai's bundle keyed by `var` — a test-unique env
/// var, so no test races another (or the developer's real `ZAI_API_KEY`).
fn catalog_keyed_by(var: &str) -> Catalog {
    let mut catalog = Catalog::builtin();
    for p in &mut catalog.providers {
        if p.name == "zai" {
            p.key_env = Some(var.into());
        }
    }
    catalog
}

fn registry() -> SharedRegistry {
    Arc::new(RwLock::new(crate::tools::ToolRegistry::new()))
}

fn active() -> ActiveServers {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Registered = available: every keyed bundle is listed, nothing connects.
#[test]
fn keyed_bundles_are_available() {
    std::env::set_var("ZAI_TEST_KEY_0226_AVAILABLE", "test-key");
    let avail = AvailableMcp::from_config(
        &catalog_keyed_by("ZAI_TEST_KEY_0226_AVAILABLE"),
        &HashMap::new(),
        vec![],
    );
    assert_eq!(
        avail.available_names(),
        vec!["web_reader", "web_search_prime", "zread"]
    );
    let zread = avail.get("zread").unwrap();
    assert_eq!(zread.provider.as_deref(), Some("zai"));
    assert!(zread.config.description.is_some());
    assert!(avail.is_bundled("zread"));
    std::env::remove_var("ZAI_TEST_KEY_0226_AVAILABLE");
}

/// Keyless ⇒ silently absent: not listed, not gettable, not "disabled".
#[test]
fn keyless_bundle_is_silently_absent() {
    let avail = AvailableMcp::from_config(
        &catalog_keyed_by("DEFINITELY_UNSET_TEST_KEY_0226"),
        &HashMap::new(),
        vec![],
    );
    assert!(avail.available_names().is_empty());
    assert!(avail.get("zread").is_none());
    assert_eq!(avail.tier_of("zread"), McpTier::Unknown);
}

/// A user `mcp:` override merges field-wise over the bundled definition:
/// `disabled: true` alone hides one bundle; a description replaces the
/// bundled one and the bundled url survives.
#[test]
fn user_override_merges_over_bundle_by_name() {
    std::env::set_var("ZAI_TEST_KEY_0226_OVR", "k");
    let off = McpServerConfig {
        url: None,
        disabled: true,
        ..http_entry("x")
    };
    let described = McpServerConfig {
        url: None,
        description: Some("my reader".into()),
        ..http_entry("x")
    };
    let user = HashMap::from([
        ("web_search_prime".to_string(), off),
        ("web_reader".to_string(), described),
    ]);
    let avail =
        AvailableMcp::from_config(&catalog_keyed_by("ZAI_TEST_KEY_0226_OVR"), &user, vec![]);
    assert_eq!(avail.available_names(), vec!["web_reader", "zread"]);
    assert_eq!(avail.tier_of("web_search_prime"), McpTier::Disabled);
    let reader = avail.get("web_reader").unwrap();
    assert_eq!(reader.config.description.as_deref(), Some("my reader"));
    assert!(reader.config.url.as_deref().unwrap().contains("web_reader"));
    std::env::remove_var("ZAI_TEST_KEY_0226_OVR");
}

/// User-declared servers are available ungated unless `disabled: true`, and a
/// live add/remove updates the roster.
#[test]
fn user_entries_are_available_unless_disabled_and_live_edits_apply() {
    let empty = Catalog {
        providers: Vec::new(),
    };
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
    let avail = AvailableMcp::from_config(&empty, &user, vec![]);
    assert_eq!(avail.available_names(), vec!["kb"]);
    assert_eq!(avail.tier_of("off"), McpTier::Disabled);
    assert!(!avail.is_bundled("kb"));
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

/// Transport-exclusive user override: setting `url` clears a bundled
/// `command` (and vice versa) so the XOR invariant holds after the merge.
#[test]
fn merge_keeps_transport_xor() {
    let mut bundled = http_entry("https://bundled.example/mcp");
    let merged = merge_user_over_bundled(bundled.clone(), &stdio("local-proxy"));
    assert_eq!(merged.command.as_deref(), Some("local-proxy"));
    assert!(merged.url.is_none());
    assert!(merged.transport().is_ok());
    bundled.url = None;
    bundled.command = Some("bundled-cmd".into());
    let merged = merge_user_over_bundled(bundled, &http_entry("https://user.example/mcp"));
    assert!(merged.command.is_none());
    assert_eq!(merged.url.as_deref(), Some("https://user.example/mcp"));
    assert!(merged.transport().is_ok());
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
    let empty = Catalog {
        providers: Vec::new(),
    };
    let user = HashMap::from([
        ("my.docs".to_string(), http_entry("https://a/mcp")),
        ("my_docs".to_string(), http_entry("https://b/mcp")),
        ("a".to_string(), http_entry("https://c/mcp")),
        ("a__b".to_string(), http_entry("https://d/mcp")),
        ("gh".to_string(), http_entry("https://e/mcp")),
    ]);
    let avail = AvailableMcp::from_config(&empty, &user, vec![]);
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

/// A server that can never connect — disabled, or a bundle whose key is unset
/// — claims no name: a clashing user server that can connect stays.
#[test]
fn a_disabled_or_keyless_server_never_knocks_out_a_clashing_one() {
    let mut catalog = catalog_keyed_by("DEFINITELY_UNSET_TEST_KEY_0226_CLASH");
    // The keyless bundle `zread`'s prefix nests the user's `zread__extra`.
    let off = McpServerConfig {
        disabled: true,
        ..http_entry("https://off/mcp")
    };
    let user = HashMap::from([
        ("zread__extra".to_string(), http_entry("https://mine/mcp")),
        ("a".to_string(), off),
        ("a__b".to_string(), http_entry("https://ab/mcp")),
    ]);
    catalog.providers.retain(|p| p.name == "zai");
    let avail = AvailableMcp::from_config(&catalog, &user, vec![]);
    let names = avail.available_names();
    assert!(names.contains(&"zread__extra".to_string()), "{names:?}");
    assert!(names.contains(&"a__b".to_string()), "{names:?}");
    assert!(
        avail.clash_warnings().is_empty(),
        "{:?}",
        avail.clash_warnings()
    );
}
