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
//! plan authorship) off `Agent` and onto the session's independent
//! permission mode; stage 5b moved spawn control, sandbox confinement and
//! the primary/subagent/all `mode` distinction off it too — an agent no
//! longer has a posture to render here at all, only identity (name,
//! description, model/provider pin, assembled prompt).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::agents::{self, AgentLayer, AgentResolution};
use crate::skills;
use crate::system_prompt::PromptContext;

/// With no `name`, print a table of every resolved agent (name, model, layer,
/// source). With a `name`, print the full resolved agent — identity,
/// prompt length — plus which lower-layer definitions it overrode.
pub fn inspect_agents(cwd: &Path, name: Option<&str>) -> Result<()> {
    let skill_registry = skills::load_registry(cwd).context("loading skill definitions")?;
    let ctx = PromptContext::load(cwd);

    let resolved =
        agents::resolve_registry(cwd, &ctx, &skill_registry).context("resolving agent registry")?;

    match name {
        Some(name) => {
            let entry = resolved
                .iter()
                .find(|r| r.agent.name == name)
                .with_context(|| {
                    format!("unknown agent `{name}` (no matching definition found)")
                })?;
            print!("{}", render_agent_detail(entry));
        }
        None => print!("{}", render_agent_table(&resolved)),
    }
    Ok(())
}

/// One-line-per-agent table: name, model, starting-mode hint, winning layer,
/// source.
/// Columns are width-fit to the widest cell.
pub(super) fn render_agent_table(resolved: &[AgentResolution]) -> String {
    if resolved.is_empty() {
        return "no agent definitions found\n".to_string();
    }

    let rows: Vec<Vec<String>> = resolved
        .iter()
        .map(|r| {
            vec![
                r.agent.name.clone(),
                r.agent.model.clone().unwrap_or_else(|| "inherit".into()),
                r.agent.default_mode.clone().unwrap_or_else(|| "-".into()),
                r.layer.label().to_string(),
                r.source.clone(),
            ]
        })
        .collect();

    super::render_table(&["NAME", "MODEL", "DEFAULT MODE", "LAYER", "SOURCE"], &rows)
}

/// Full resolved definition for one agent: identity/provenance and the
/// assembled-prompt length. Permission/mask/plan-authorship/spawn-control/
/// sandbox posture is gone (ADR-0207): it lives on the session's permission
/// mode now, not this agent — any agent is a valid spawn target (§6).
pub(super) fn render_agent_detail(entry: &AgentResolution) -> String {
    let p = &entry.agent;
    let mut out = String::new();
    let _ = writeln!(out, "name:        {}", p.name);
    let _ = writeln!(out, "description: {}", p.description);
    let _ = writeln!(
        out,
        "model:       {}",
        p.model.as_deref().unwrap_or("inherit (session default)")
    );
    // A starting-mode hint only (ADR-0218): `--mode`/config `mode:` win.
    let _ = writeln!(
        out,
        "default_mode: {}",
        p.default_mode
            .as_deref()
            .unwrap_or("none (session default)")
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
    use entanglement_core::Agent;

    fn resolution(agent: Agent) -> AgentResolution {
        AgentResolution {
            agent,
            layer: AgentLayer::BuiltIn,
            source: "built-in (t.md)".into(),
            shadowed: Vec::new(),
        }
    }

    fn agent() -> Agent {
        Agent {
            name: "t".into(),
            description: String::new(),
            system_prompt: "prompt body".into(),
            model: None,
            provider: None,
            default_mode: None,
        }
    }

    #[test]
    fn table_lists_name_model_layer_source() {
        let rows = vec![resolution(agent())];
        let table = render_agent_table(&rows);
        assert!(table.contains("NAME"), "{table}");
        assert!(table.contains("MODEL"), "{table}");
        assert!(table.contains("DEFAULT MODE"), "{table}");
        assert!(table.contains("LAYER"), "{table}");
        assert!(table.contains("SOURCE"), "{table}");
        assert!(!table.contains("MASK"), "{table}");
        assert!(!table.contains("DISPATCH"), "{table}");
    }

    #[test]
    fn detail_shows_identity_no_permission_or_spawn_posture() {
        let detail = render_agent_detail(&resolution(agent()));
        assert!(detail.contains("name:        t"), "{detail}");
        assert!(
            detail.contains("assembled system prompt: 11 chars"),
            "{detail}"
        );
        // Only the starting-mode hint, never a permission mode of its own.
        assert!(!detail.lines().any(|l| l.starts_with("mode:")), "{detail}");
        assert!(
            detail.contains("default_mode: none (session default)"),
            "{detail}"
        );
        assert!(!detail.contains("spawn control"), "{detail}");
        assert!(!detail.contains("tool mask"), "{detail}");
        assert!(!detail.contains("permission"), "{detail}");
        assert!(!detail.contains("dispatch state"), "{detail}");
    }

    #[test]
    fn table_and_detail_show_the_default_mode_hint() {
        let mut a = agent();
        a.default_mode = Some("plan".into());
        let rows = vec![resolution(a)];
        assert!(render_agent_table(&rows).contains("plan"));
        assert!(render_agent_detail(&rows[0]).contains("default_mode: plan"));
    }

    #[test]
    fn detail_reports_no_lower_layer_override_by_default() {
        let detail = render_agent_detail(&resolution(agent()));
        assert!(
            detail.contains("overrides:   (none — no lower-layer definition)"),
            "{detail}"
        );
    }
}
