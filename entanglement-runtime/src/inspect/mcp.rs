//! `skutter inspect mcp` (#558, closes deferred-work-ledger row 10c).
//!
//! Surfaces the *resolved* MCP server universe — provider-bundled servers
//! (#542) folded with the user's `mcp:` config entries — without spawning the
//! engine or connecting to anything: which servers auto-connect at session
//! start (`enabled`), which wait for a per-session `/enable` (`allowed`), the
//! bundling provider (`None` for a user-declared server), and whether an
//! OAuth-protected one already has a stored credential. This is the static
//! availability picture; live connection status is `/mcp list`'s job.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::Catalog;

use crate::config::{Config, McpTokenStore};
use crate::mcp::{transport_label, AvailableMcp};

/// Resolve the catalog + user config for `cwd` and print the bundled/available
/// MCP server roster.
pub fn inspect_mcp(cwd: &Path) -> Result<()> {
    let catalog = Catalog::load().context("loading provider catalog")?;
    let resolved = Config::resolve(cwd).context("resolving user config")?;
    let token_store = McpTokenStore::load();
    print!("{}", render_mcp(&catalog, &resolved.config, &token_store));
    Ok(())
}

fn render_mcp(catalog: &Catalog, config: &Config, tokens: &McpTokenStore) -> String {
    let provider_of: HashMap<String, String> = catalog
        .providers
        .iter()
        .flat_map(|p| {
            p.mcp_servers
                .keys()
                .map(move |name| (name.clone(), p.name.clone()))
        })
        .collect();

    let secret_env = catalog.key_envs();
    let (startup, avail) = AvailableMcp::partition(catalog, &config.mcp, secret_env);

    let mut out = String::new();
    let _ = writeln!(out, "startup-connected (state: enabled):");
    if startup.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        let mut names: Vec<&String> = startup.keys().collect();
        names.sort();
        for name in names {
            let cfg = &startup[name];
            let provider = provider_of
                .get(name)
                .map(String::as_str)
                .unwrap_or("user-defined");
            let _ = writeln!(out, "  {name}: {provider} ({})", transport_label(cfg));
        }
    }

    let _ = writeln!(
        out,
        "\navailable (state: allowed — needs /enable mcp <name>):"
    );
    let available_names = avail.available_names();
    if available_names.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for name in &available_names {
            let Some(server) = avail.get(name) else {
                continue;
            };
            let provider = server.provider.as_deref().unwrap_or("user-defined");
            let auth = if server.config.oauth.is_some() {
                if tokens.has(name) {
                    " [oauth: authenticated]"
                } else {
                    " [oauth: needs auth]"
                }
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "  {name}: {provider} ({}){auth}",
                transport_label(&server.config)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{Permission, PermissionProfile};

    fn empty_catalog() -> Catalog {
        Catalog { providers: vec![] }
    }

    fn empty_config() -> Config {
        Config {
            agent: None,
            provider: None,
            model: None,
            verbose: false,
            permissions: PermissionProfile::new(Permission::Allow),
            hooks: Default::default(),
            mcp: HashMap::new(),
            web_search: Default::default(),
            max_turns: None,
            idle_ttl: None,
            auto_compact: None,
            editor: None,
            session_retention_days: 30,
        }
    }

    #[test]
    fn empty_config_reports_no_startup_or_available_servers() {
        let catalog = empty_catalog();
        let config = empty_config();
        let tokens = McpTokenStore::empty();
        let rendered = render_mcp(&catalog, &config, &tokens);
        assert!(rendered.contains("startup-connected (state: enabled):\n  (none)"));
        assert!(rendered.contains("available (state: allowed"));
        assert!(rendered.contains("  (none)"));
    }

    #[test]
    fn a_user_allowed_server_shows_up_as_user_defined_and_available() {
        let catalog = empty_catalog();
        let mut config = empty_config();
        config.mcp.insert(
            "docs".to_string(),
            crate::mcp::McpServerConfig {
                command: Some("docs-server".to_string()),
                args: vec![],
                env: HashMap::new(),
                url: None,
                headers: HashMap::new(),
                disabled: false,
                capabilities: HashMap::new(),
                oauth: None,
                state: Some(entanglement_core::McpServerState::Allowed),
            },
        );
        let tokens = McpTokenStore::empty();
        let rendered = render_mcp(&catalog, &config, &tokens);
        assert!(rendered.contains("docs: user-defined (stdio)"));
    }
}
