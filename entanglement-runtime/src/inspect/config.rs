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
/// winning layer, the permission ceiling, and the discovered layer sources.
pub fn inspect_config(cwd: &Path) -> Result<()> {
    let resolved = Config::resolve(cwd).context("resolving user config")?;
    print!("{}", render_config(&resolved));
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
        "\npermissions ceiling (← {}, last matching rule wins):",
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
    out
}
