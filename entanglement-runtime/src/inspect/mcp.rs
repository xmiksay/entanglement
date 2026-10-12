//! `skutter inspect mcp` (#558).
//!
//! Surfaces the *resolved* MCP server roster — provider-bundled servers
//! (#542) folded with the user's `mcp:` config entries — without spawning the
//! engine or connecting to anything: every registered server (ADR-0226:
//! registered = available, nothing connects at startup), its bundling
//! provider, its description, whether an OAuth-protected one already has a
//! stored credential, and the last-seen tool names. This is the static
//! picture; live connection status is `/mcp list`'s job.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::Catalog;

use crate::config::{Config, McpTokenStore};
use crate::mcp::{transport_label, AvailableMcp, McpToolCache};

/// Resolve the catalog + user config for `cwd` and print the bundled/available
/// MCP server roster.
pub fn inspect_mcp(cwd: &Path) -> Result<()> {
    let catalog = Catalog::load().context("loading provider catalog")?;
    let resolved = Config::resolve(cwd).context("resolving user config")?;
    let token_store = McpTokenStore::load();
    let cache = McpToolCache::managed();
    print!(
        "{}",
        render_mcp(&catalog, &resolved.config, &token_store, &cache)
    );
    Ok(())
}

fn render_mcp(
    catalog: &Catalog,
    config: &Config,
    tokens: &McpTokenStore,
    cache: &McpToolCache,
) -> String {
    let avail = AvailableMcp::from_config(catalog, &config.mcp, catalog.key_envs());
    let mut out = String::new();
    let _ = writeln!(
        out,
        "registered MCP servers (available; connect on first mcp_tools/mcp call):"
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
            if let Some(d) = &server.config.description {
                let _ = writeln!(out, "    {d}");
            }
            if let Some(tools) = cache.lookup(name, &server.config.target()) {
                let _ = writeln!(out, "    tools (last seen): {}", tools.join(", "));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{Permission, PermissionProfile};
    use std::collections::HashMap;

    fn empty_catalog() -> Catalog {
        Catalog { providers: vec![] }
    }

    fn empty_config() -> Config {
        Config {
            agent: None,
            mode: None,
            provider: None,
            model: None,
            verbose: false,
            permissions: PermissionProfile::new(Permission::Allow),
            modes: HashMap::new(),
            hooks: Default::default(),
            mcp: HashMap::new(),
            endpoints: HashMap::new(),
            web_search: Default::default(),
            max_turns: None,
            idle_ttl: None,
            auto_compact: None,
            editor: None,
            session_retention_days: 30,
            tui: Default::default(),
            undo: Default::default(),
        }
    }

    #[test]
    fn empty_config_reports_no_servers() {
        let rendered = render_mcp(
            &empty_catalog(),
            &empty_config(),
            &McpTokenStore::at(None),
            &McpToolCache::at(None),
        );
        assert!(rendered.contains("registered MCP servers"), "{rendered}");
        assert!(rendered.contains("  (none)"), "{rendered}");
    }

    #[test]
    fn a_user_server_shows_up_with_its_description_and_last_seen_tools() {
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
                description: Some("project docs".to_string()),
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let cache = McpToolCache::at(Some(dir.path().join("mcp-tools.yml")));
        cache
            .record("docs", "docs-server", &["search".into()])
            .unwrap();
        let rendered = render_mcp(&catalog, &config, &McpTokenStore::at(None), &cache);
        assert!(
            rendered.contains("docs: user-defined (stdio)"),
            "{rendered}"
        );
        assert!(rendered.contains("    project docs"), "{rendered}");
        assert!(rendered.contains("tools (last seen): search"), "{rendered}");
    }
}
