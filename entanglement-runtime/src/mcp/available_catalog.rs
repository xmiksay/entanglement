//! Catalog-bundled server behavior beyond `available.rs`'s own per-server
//! merge: folding a bundled server's own `capabilities` hint into the shared
//! permission capability index. Sibling file (`#[path]` child module, so
//! private helpers in `available.rs` stay reachable) to keep both sides of
//! the 400-line file cap.

use super::*;

/// Every catalog-bundled server's config, merged with any same-name user
/// `mcp:` override the same way [`AvailableMcp::from_config`] merges one
/// (`bundled_config` + `merge_user_over_bundled`) — for
/// [`super::super::capability_index_with_catalog`], which needs a bundled
/// server's own `capabilities` hint (e.g. z.ai's `web_search_prime` →
/// `read`) even though the server itself never joins `user_mcp` (#542).
/// `disabled` is irrelevant here: a capability hint for a tool that never
/// connects is simply inert (`capability_index`'s own doc).
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

    /// A bundled server's capability hint reaches the index even when the
    /// server is disabled — a hint for a tool that never connects is inert.
    #[test]
    fn every_bundled_server_contributes_its_config() {
        let mut user = HashMap::new();
        user.insert(
            "zread".to_string(),
            McpServerConfig {
                disabled: true,
                ..super::super::tests::stdio("ignored")
            },
        );
        let configs = bundled_capability_configs(&Catalog::builtin(), &user);
        assert_eq!(configs.len(), 3);
        assert!(configs["zread"].disabled);
        assert_eq!(
            configs["web_search_prime"].capabilities["webSearchPrime"],
            "read"
        );
    }
}
