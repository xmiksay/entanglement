//! `skutter inspect config` (#172).
//!
//! Surfaces the resolved user configuration without spawning the engine: the
//! merged settings, which layer won each field (default < user < project), and
//! the discovered layer files. Closes the "did my `~/.config` value actually
//! win, or did the repo override it?" blind spot for the settings file, mirroring
//! `inspect agents`/`skills`.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{Config, ConfigLayer, Resolved};

/// Resolve the user config for `cwd` and print the merged values with their
/// winning layer, the permission ceiling, the discovered layer sources, and
/// the catalog's advanced wire facts (ADR-0213).
pub fn inspect_config(cwd: &Path) -> Result<()> {
    let resolved = Config::resolve(cwd).context("resolving user config")?;
    let mut out = render_config(&resolved);
    match entanglement_core::Catalog::load() {
        Ok(catalog) => super::catalog_facts::render(&mut out, &catalog),
        Err(e) => {
            let _ = writeln!(
                out,
                "
advanced catalog: (failed to load: {e:#})"
            );
        }
    }
    print!("{out}");
    Ok(())
}

/// Render the resolved config: discovered layers, per-field values + provenance,
/// and the permission ceiling.
fn render_config(resolved: &Resolved) -> String {
    let c = &resolved.config;
    let prov: std::collections::HashMap<&str, ConfigLayer> = resolved
        .provenance
        .iter()
        .map(|(k, l)| (k.as_str(), *l))
        .collect();
    // The winning layer for a field, or `default` when no layer set it (falls
    // back to the embedded default, which always defines every key).
    let from = |key: &str| {
        prov.get(key)
            .copied()
            .unwrap_or(ConfigLayer::Default)
            .label()
    };

    let mut out = String::new();

    let _ = writeln!(out, "layers (low → high precedence):");
    for (layer, source) in &resolved.layers {
        let _ = writeln!(out, "  {:<8} {}", layer.label(), source);
    }

    let _ = writeln!(out, "\nsettings (value ← winning layer):");
    let _ = writeln!(
        out,
        "  agent:    {:<12} ← {}",
        c.agent.as_deref().unwrap_or("(none)"),
        from("agent")
    );
    let _ = writeln!(
        out,
        "  provider: {:<12} ← {}",
        c.provider.as_deref().unwrap_or("(auto-detect)"),
        from("provider")
    );
    let _ = writeln!(
        out,
        "  model:    {:<12} ← {}",
        c.model.as_deref().unwrap_or("(provider default)"),
        from("model")
    );
    let _ = writeln!(out, "  verbose:  {:<12} ← {}", c.verbose, from("verbose"));
    let _ = writeln!(
        out,
        "  max_turns: {:<12} ← {}",
        c.max_turns
            .map(|n| n.to_string())
            .unwrap_or_else(|| "(engine default: 200)".to_string()),
        from("max_turns")
    );
    let _ = writeln!(
        out,
        "  idle_ttl_secs: {:<12} ← {}",
        c.idle_ttl
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|| "(disabled — no auto-hibernation sweep)".to_string()),
        from("idle_ttl_secs")
    );
    let _ = writeln!(
        out,
        "  auto_compact: {:<12} ← {}",
        c.auto_compact
            .map(|b| b.to_string())
            .unwrap_or_else(|| "(engine default: true)".to_string()),
        from("auto_compact")
    );
    let _ = writeln!(
        out,
        "  editor:   {:<12} ← {}",
        c.editor.as_deref().unwrap_or("($VISUAL → $EDITOR → vi)"),
        from("editor")
    );
    let _ = writeln!(
        out,
        "  session_retention_days: {:<12} ← {}",
        c.session_retention_days,
        from("session_retention_days")
    );
    // Tool advertising (ADR-0196): the global view resolves the config tier
    // only — the catalog tier is per model (`tool_advertising:` in
    // providers.yml), so name that fact rather than pretending this line is
    // the whole answer.
    let mode = c.tool_advertising.unwrap_or_default();
    let _ = writeln!(
        out,
        "  tool_advertising: {:<12} ← {} (per-model catalog \
         `tool_advertising:` preference applies when unset; resolved per \
         session at start)",
        mode.label(),
        if c.tool_advertising.is_some() {
            from("tool_advertising")
        } else {
            "default"
        }
    );
    let _ = writeln!(
        out,
        "  tui:      notify={} mouse={} ← {}",
        c.tui.notify,
        c.tui.mouse,
        from("tui")
    );

    let _ = writeln!(
        out,
        "\npermissions ceiling (← {}, longest matching rule wins, same grammar \
         as a permission mode — see `inspect modes`):",
        from("permissions")
    );
    let _ = writeln!(out, "  default: {:?}", c.permissions.default);
    if c.permissions.rules.is_empty() {
        let _ = writeln!(out, "  (no per-tool rules)");
    } else {
        for (pat, perm) in &c.permissions.rules {
            let _ = writeln!(out, "  {pat}: {perm:?}");
        }
    }

    let _ = writeln!(out, "\nhooks (← {}):", from("hooks"));
    if c.hooks.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        render_hook_list(&mut out, "pre_tool_use", &c.hooks.pre_tool_use);
        render_hook_list(&mut out, "post_tool_use", &c.hooks.post_tool_use);
        render_hook_list(&mut out, "user_prompt_submit", &c.hooks.user_prompt_submit);
    }

    let _ = writeln!(out, "\nmcp servers (← {}):", from("mcp"));
    if c.mcp.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        // Stable order — the config map's iteration order is unspecified.
        let mut names: Vec<&String> = c.mcp.keys().collect();
        names.sort();
        for name in names {
            let s = &c.mcp[name];
            let state = if s.disabled { " (disabled)" } else { "" };
            // Describe whichever transport the block resolves to; a `command`/`url`
            // XOR violation surfaces here rather than the raw fields.
            let transport = match s.transport() {
                Ok(crate::mcp::Transport::Stdio { command, args, .. }) => {
                    format!("{command} {}", args.join(" "))
                        .trim_end()
                        .to_string()
                }
                Ok(crate::mcp::Transport::Http { url, .. }) => format!("http {url}"),
                Err(e) => format!("(invalid: {e})"),
            };
            let _ = writeln!(out, "  {name}{state}: {transport}");
        }
    }

    let _ = writeln!(out, "\nweb search (← {}):", from("web_search"));
    let ws = &c.web_search;
    if !ws.enabled {
        let _ = writeln!(out, "  disabled");
    } else {
        let max = ws
            .max_uses
            .map(|m| m.to_string())
            .unwrap_or_else(|| "provider default".to_string());
        let domains = if ws.allowed_domains.is_empty() {
            "any".to_string()
        } else {
            ws.allowed_domains.join(", ")
        };
        let _ = writeln!(
            out,
            "  enabled (max_uses: {max}, allowed_domains: {domains})"
        );
    }
    if !resolved.warnings.is_empty() {
        let _ = writeln!(
            out,
            "
warnings:"
        );
        for w in &resolved.warnings {
            let _ = writeln!(out, "  {w}");
        }
    }
    out
}

/// Render one lifecycle point's configured hooks: each command with its optional
/// tool filter. Skips a point with no hooks so only the active ones show.
fn render_hook_list(out: &mut String, point: &str, hooks: &[crate::hooks::HookSpec]) {
    if hooks.is_empty() {
        return;
    }
    let _ = writeln!(out, "  {point}:");
    for h in hooks {
        let scope = if h.tools.is_empty() {
            String::new()
        } else {
            format!("  [tools: {}]", h.tools.join(", "))
        };
        let _ = writeln!(out, "    - {}{scope}", h.command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ENTANGLEMENT_CONFIG_FILE` is process-global; shared with every other test in this crate that touches
    /// them (`config::tests`, `config::tests_retention`, `extra_roots`) via
    /// `crate::config::ENV_LOCK` rather than a module-local lock.
    fn locked_resolve(root: &std::path::Path) -> Resolved {
        std::env::set_var("ENTANGLEMENT_CONFIG_FILE", root.join("nope.yml"));
        let resolved = Config::resolve(root).unwrap();
        std::env::remove_var("ENTANGLEMENT_CONFIG_FILE");
        resolved
    }

    #[test]
    fn embedded_defaults_report_the_default_layer_and_engine_fallback_text() {
        let _g = crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let resolved = locked_resolve(dir.path());
        let rendered = render_config(&resolved);

        assert!(rendered.contains("max_turns: 200") && rendered.contains("← default"));
        assert!(rendered.contains("idle_ttl_secs: (disabled — no auto-hibernation sweep)"));
        assert!(rendered.contains("auto_compact: true"));
        assert!(rendered.contains("editor:   ($VISUAL → $EDITOR → vi)"));
        assert!(rendered.contains("session_retention_days: 30") && rendered.contains("← default"));
    }

    #[test]
    fn user_layer_overrides_report_their_winning_layer() {
        let _g = crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let user_file = dir.path().join("user-config.yml");
        std::fs::write(
            &user_file,
            "max_turns: 50\nidle_ttl_secs: 120\nauto_compact: false\neditor: \"code --wait\"\n",
        )
        .unwrap();

        std::env::set_var("ENTANGLEMENT_CONFIG_FILE", &user_file);
        let resolved = Config::resolve(dir.path()).unwrap();
        std::env::remove_var("ENTANGLEMENT_CONFIG_FILE");

        let rendered = render_config(&resolved);
        assert!(rendered.contains("max_turns: 50") && rendered.contains("← user"));
        assert!(rendered.contains("idle_ttl_secs: 120") && rendered.contains("← user"));
        assert!(rendered.contains("auto_compact: false") && rendered.contains("← user"));
        assert!(rendered.contains("editor:   code --wait") && rendered.contains("← user"));
    }

    #[test]
    fn tui_block_and_retired_key_warnings_are_rendered() {
        let _g = crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let user_file = dir.path().join("user-config.yml");
        std::fs::write(
            &user_file,
            "tui: {notify: true}\ndiscovery: {zai: invoke}\n",
        )
        .unwrap();
        std::env::set_var("ENTANGLEMENT_CONFIG_FILE", &user_file);
        let resolved = Config::resolve(dir.path()).unwrap();
        std::env::remove_var("ENTANGLEMENT_CONFIG_FILE");

        let rendered = render_config(&resolved);
        assert!(
            rendered.contains("tui:      notify=true mouse=true ← user"),
            "{rendered}"
        );
        assert!(
            rendered.contains("warnings:") && rendered.contains("`discovery:`"),
            "{rendered}"
        );
    }
}
