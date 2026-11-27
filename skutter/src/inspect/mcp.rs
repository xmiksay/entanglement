//! `skutter inspect mcp` (#558).
//!
//! Surfaces the *resolved* MCP server roster — the user's `mcp:` config
//! entries — without spawning the engine or connecting to anything: every
//! registered server (ADR-0226: registered = available, nothing connects at
//! startup), its transport, its description, whether an OAuth-protected one already has a
//! stored credential, and the last-seen tool names. This is the static
//! picture; live connection status is `/mcp list`'s job.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::mcp::{transport_label, AvailableMcp, McpToolCache};
use crate::state::{FileMcpToolCache, McpTokenStore};

/// Resolve the user config for `cwd` and print the available MCP server
/// roster.
pub fn inspect_mcp(cwd: &Path) -> Result<()> {
    let resolved = Config::resolve(cwd).context("resolving user config")?;
    let token_store = McpTokenStore::load();
    let cache = FileMcpToolCache::managed();
    print!("{}", render_mcp(&resolved.config, &token_store, &cache));
    Ok(())
}

fn render_mcp(config: &Config, tokens: &McpTokenStore, cache: &dyn McpToolCache) -> String {
    let avail = AvailableMcp::from_config(&config.mcp, Vec::new());
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
            let auth = if server.oauth.is_some() {
                if tokens.has(name) {
                    " [oauth: authenticated]"
                } else {
                    " [oauth: needs auth]"
                }
            } else {
                ""
            };
            let _ = writeln!(out, "  {name} ({}){auth}", transport_label(&server));
            if let Some(d) = &server.description {
                let _ = writeln!(out, "    {d}");
            }
            if let Some(tools) = cache.lookup(name, &server.target()) {
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
            http_auth: Default::default(),
            untrusted_http_rules: Default::default(),
            rhai_trusted: Default::default(),
            scoped_rules: Vec::new(),
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
            &empty_config(),
            &McpTokenStore::at(None),
            &FileMcpToolCache::at(None),
        );
        assert!(rendered.contains("registered MCP servers"), "{rendered}");
        assert!(rendered.contains("  (none)"), "{rendered}");
    }

    #[test]
    fn a_user_server_shows_up_with_its_description_and_last_seen_tools() {
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
        let cache = FileMcpToolCache::at(Some(dir.path().join("mcp-tools.yml")));
        cache
            .record("docs", "docs-server", &["search".into()])
            .unwrap();
        let rendered = render_mcp(&config, &McpTokenStore::at(None), &cache);
        assert!(rendered.contains("  docs (stdio)"), "{rendered}");
        assert!(rendered.contains("    project docs"), "{rendered}");
        assert!(rendered.contains("tools (last seen): search"), "{rendered}");
    }
}
