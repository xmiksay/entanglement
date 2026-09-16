//! Catalog-bundled server behavior beyond `available.rs`'s own per-server
//! merge (the explore/research provider-bundled-MCP fix): folding a bundled
//! server's own `capabilities` hint into the shared permission capability
//! index, and pinning that ADR-0152's three-state tier — not a calling
//! profile's own grade — is what actually gates `mcp_enable`. Sibling file
//! (`#[path]` child module, so private helpers in `available.rs` stay
//! reachable) to keep both sides of the 400-line file cap.

use super::*;

/// Every catalog-bundled server's config, merged with any same-name user
/// `mcp:` override the same way [`AvailableMcp::partition`] merges one
/// (`bundled_config` + `merge_user_over_bundled`) — for
/// [`super::super::capability_index_with_catalog`], which needs a bundled
/// server's own `capabilities` hint (e.g. z.ai's `web_search_prime` →
/// `read`) even though the server itself never joins `user_mcp` (#542).
/// State/enablement is irrelevant here: a capability hint for a tool that
/// never connects is simply inert (`capability_index`'s own doc), so this
/// covers every bundled server regardless of its
/// `allowed`/`enabled`/`disabled` tier.
pub fn bundled_capability_configs(
    catalog: &Catalog,
    user_mcp: &HashMap<String, McpServerConfig>,
) -> HashMap<String, McpServerConfig> {
    let mut out = HashMap::new();
    for provider in &catalog.providers {
        for (name, bundled) in &provider.mcp_servers {
            let mut cfg = bundled_config(bundled);
            if let Some(user) = user_mcp.get(name) {
                cfg = merge_user_over_bundled(cfg, user);
            }
            out.insert(name.clone(), cfg);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::live::ActiveServers;
    use crate::tools::SharedRegistry;
    use std::sync::Mutex;

    /// ADR-0152's three-state tier is the real consent boundary for
    /// `mcp_enable` (the explore/research profile grade is `allow` outright
    /// — see `agents::mod::tests::
    /// explore_and_research_can_enable_and_use_a_read_hinted_bundled_mcp_tool`):
    /// a bundled server demoted to `disabled` still refuses enablement
    /// exactly like an unknown name, no matter how permissive the calling
    /// profile is — `AvailableMcp::get` makes a `disabled` and an unknown
    /// server indistinguishable by design (`available.rs`'s module docs), so
    /// `mcp_enable` cannot punch through the tier from the tool-call side.
    #[tokio::test]
    async fn enable_refuses_a_server_demoted_to_disabled_tier() {
        let mut cat = Catalog::builtin();
        for p in &mut cat.providers {
            if p.name == "zai" {
                p.key_env = Some("ZAI_TEST_KEY_TIER_GATE".into());
            }
        }
        std::env::set_var("ZAI_TEST_KEY_TIER_GATE", "test-key");
        let mut user_mcp = HashMap::new();
        user_mcp.insert(
            "web_search_prime".to_string(),
            McpServerConfig {
                command: None,
                args: vec![],
                env: HashMap::new(),
                url: Some("ignored".to_string()),
                headers: HashMap::new(),
                disabled: false,
                capabilities: HashMap::new(),
                oauth: None,
                state: Some(McpServerState::Disabled),
            },
        );
        let (_startup, avail) = AvailableMcp::partition(&cat, &user_mcp, vec![]);
        std::env::remove_var("ZAI_TEST_KEY_TIER_GATE");
        // Demoted out of the available roster entirely — same as never existing.
        assert!(avail.get("web_search_prime").is_none());

        let registry: SharedRegistry =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tools::ToolRegistry::new()));
        let active: ActiveServers = std::sync::Arc::new(Mutex::new(HashMap::new()));
        let http = entanglement_core::HttpClient::new().unwrap();
        let err = enable_for_session(
            &avail,
            "web_search_prime",
            &SessionId::new("s"),
            &registry,
            &active,
            &http,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no available MCP server"), "{err}");
    }
}
