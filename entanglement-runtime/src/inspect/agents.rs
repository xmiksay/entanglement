//! `skutter inspect agents [name]` (#185).
//!
//! Closes the layer-collision blind spot: three layers (built-in < user <
//! project) merge by `name` with a silent later-wins `insert`, so a user could
//! not tell whether their `~/.config` `build.md` actually won, nor what the
//! final mode is. It reuses the same discovery as startup, keeps the
//! provenance the `insert` swallows ([`crate::agents::resolve_registry`]), and
//! prints a table or a per-agent detail view.
//!
//! ADR-0207 moved every permission fact (the tool mask, the permission rules,
//! plan authorship) off `AgentProfile` and onto the session's independent
//! permission mode; stage 5b moved spawn control, sandbox confinement and
//! the primary/subagent/all `mode` distinction off it too — a profile no
//! longer has a posture to render here at all, only identity (name,
//! description, model/provider pin, assembled prompt).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::agents::{self, AgentLayer, AgentResolution};
use crate::skills;
use crate::system_prompt::PromptContext;

/// With no `name`, print a table of every resolved agent (name, model, layer,
/// source). With a `name`, print the full resolved profile — identity,
/// prompt length — plus which lower-layer definitions it overrode.
pub fn inspect_agents(cwd: &Path, name: Option<&str>) -> Result<()> {
    let skill_registry = skills::load_registry(cwd).context("loading skill definitions")?;
    let mut ctx = PromptContext::load(cwd);
    ctx.skills = skill_registry.disclosures();

    let resolved =
        agents::resolve_registry(cwd, &ctx, &skill_registry).context("resolving agent registry")?;

    match name {
        Some(name) => {
            let entry = resolved
                .iter()
                .find(|r| r.profile.name == name)
                .with_context(|| {
                    format!("unknown agent `{name}` (no matching definition found)")
                })?;
            print!("{}", render_agent_detail(entry));
        }
        None => print!("{}", render_agent_table(&resolved)),
    }
    Ok(())
}

/// One-line-per-agent table: name, mode, model, winning layer, source.
/// Columns are width-fit to the widest cell.
pub(super) fn render_agent_table(resolved: &[AgentResolution]) -> String {
    if resolved.is_empty() {
        return "no agent definitions found\n".to_string();
    }

    let rows: Vec<Vec<String>> = resolved
        .iter()
        .map(|r| {
            vec![
                r.profile.name.clone(),
                r.profile.model.clone().unwrap_or_else(|| "inherit".into()),
                r.layer.label().to_string(),
                r.source.clone(),
            ]
        })
        .collect();

    super::render_table(&["NAME", "MODEL", "LAYER", "SOURCE"], &rows)
}

/// Full resolved profile for one agent: identity/provenance and the
/// assembled-prompt length. Permission/mask/plan-authorship/spawn-control/
/// sandbox posture is gone (ADR-0207): it lives on the session's permission
/// mode now, not this profile — any agent is a valid spawn target (§6).
pub(super) fn render_agent_detail(entry: &AgentResolution) -> String {
    let p = &entry.profile;
    let mut out = String::new();
    let _ = writeln!(out, "name:        {}", p.name);
    let _ = writeln!(out, "description: {}", p.description);
    let _ = writeln!(
        out,
        "model:       {}",
        p.model.as_deref().unwrap_or("inherit (session default)")
    );
    let _ = writeln!(out, "layer:       {}", entry.layer.label());
    let _ = writeln!(out, "source:      {}", entry.source);

    if entry.shadowed.is_empty() {
        let _ = writeln!(out, "overrides:   (none — no lower-layer definition)");
    } else {
        let _ = writeln!(out, "overrides:");
        for (layer, source) in &entry.shadowed {
            // The built-in `source` already reads `built-in (build.md)`; only a
            // file-path source needs its layer prefixed so it doesn't double up.
            if *layer == AgentLayer::BuiltIn {
                let _ = writeln!(out, "  - {source}");
            } else {
                let _ = writeln!(out, "  - {} ({source})", layer.label());
            }
        }
    }

    let _ = writeln!(
        out,
        "\nassembled system prompt: {} chars",
        p.system_prompt.len()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::AgentProfile;

    fn resolution(profile: AgentProfile) -> AgentResolution {
        AgentResolution {
            profile,
            layer: AgentLayer::BuiltIn,
            source: "built-in (t.md)".into(),
            shadowed: Vec::new(),
        }
    }

    fn profile() -> AgentProfile {
        AgentProfile {
            name: "t".into(),
            description: String::new(),
            system_prompt: "prompt body".into(),
            model: None,
            provider: None,
        }
    }

    #[test]
    fn table_lists_name_model_layer_source() {
        let rows = vec![resolution(profile())];
        let table = render_agent_table(&rows);
        assert!(table.contains("NAME"), "{table}");
        assert!(table.contains("MODEL"), "{table}");
        assert!(table.contains("LAYER"), "{table}");
        assert!(table.contains("SOURCE"), "{table}");
        assert!(!table.contains("MASK"), "{table}");
        assert!(!table.contains("DISPATCH"), "{table}");
    }

    #[test]
    fn detail_shows_identity_no_permission_or_spawn_posture() {
        let detail = render_agent_detail(&resolution(profile()));
        assert!(detail.contains("name:        t"), "{detail}");
        assert!(
            detail.contains("assembled system prompt: 11 chars"),
            "{detail}"
        );
        assert!(!detail.contains("mode:"), "{detail}");
        assert!(!detail.contains("spawn control"), "{detail}");
        assert!(!detail.contains("tool mask"), "{detail}");
        assert!(!detail.contains("permission"), "{detail}");
        assert!(!detail.contains("dispatch state"), "{detail}");
    }

    #[test]
    fn detail_reports_no_lower_layer_override_by_default() {
        let detail = render_agent_detail(&resolution(profile()));
        assert!(
            detail.contains("overrides:   (none — no lower-layer definition)"),
            "{detail}"
        );
    }
}
