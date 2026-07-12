//! `skutter inspect agents [name]` (#185).
//!
//! Closes the layer-collision blind spot: three layers (built-in < user <
//! project) merge by `name` with a silent later-wins `insert`, so a user could
//! not tell whether their `~/.config` `build.md` actually won, nor what the final
//! permission/mask/mode is. It reuses the same discovery as startup, keeps the
//! provenance the `insert` swallows ([`crate::agents::resolve_registry`]), and
//! prints a table or a per-agent detail view.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::AgentProfile;

use crate::agents::{self, AgentLayer, AgentResolution};
use crate::skills;
use crate::system_prompt::PromptContext;

/// With no `name`, print a table of every resolved agent (name, mode, model,
/// layer, source, mask). With a `name`, print the full resolved profile —
/// permission rules, tool mask, spawn control, plan authority, prompt length —
/// plus which lower-layer definitions it overrode.
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

/// One-line-per-agent table: name, mode, model, winning layer, source, and a
/// compact tool-mask summary. Columns are width-fit to the widest cell.
pub(super) fn render_agent_table(resolved: &[AgentResolution]) -> String {
    if resolved.is_empty() {
        return "no agent definitions found\n".to_string();
    }

    let rows: Vec<Vec<String>> = resolved
        .iter()
        .map(|r| {
            vec![
                r.profile.name.clone(),
                format!("{:?}", r.profile.mode).to_lowercase(),
                r.profile.model.clone().unwrap_or_else(|| "inherit".into()),
                r.layer.label().to_string(),
                r.source.clone(),
                mask_summary(&r.profile),
            ]
        })
        .collect();

    super::render_table(&["NAME", "MODE", "MODEL", "LAYER", "SOURCE", "MASK"], &rows)
}

/// A compact tool-mask summary for the table: `all` when unrestricted, otherwise
/// the allowlist and/or denylist (`allow:[…] deny:[…]`).
fn mask_summary(profile: &AgentProfile) -> String {
    let mut parts = Vec::new();
    if let Some(allow) = &profile.tools {
        parts.push(format!("allow:[{}]", allow.join(",")));
    }
    if !profile.disallowed_tools.is_empty() {
        parts.push(format!("deny:[{}]", profile.disallowed_tools.join(",")));
    }
    if parts.is_empty() {
        "all".to_string()
    } else {
        parts.join(" ")
    }
}

/// Full resolved profile for one agent: identity/provenance, permission rules,
/// tool mask, spawn control, plan authority, and the assembled-prompt length —
/// the exact fields #116/#119/#140 enforcement hinges on.
pub(super) fn render_agent_detail(entry: &AgentResolution) -> String {
    let p = &entry.profile;
    let mut out = String::new();
    let _ = writeln!(out, "name:        {}", p.name);
    let _ = writeln!(out, "description: {}", p.description);
    let _ = writeln!(out, "mode:        {:?}", p.mode);
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

    let _ = writeln!(out, "\npermission (last matching rule wins):");
    let _ = writeln!(out, "  default: {:?}", p.permission.default);
    if p.permission.rules.is_empty() {
        let _ = writeln!(out, "  (no per-tool rules)");
    } else {
        for (pat, perm) in &p.permission.rules {
            let _ = writeln!(out, "  {pat}: {perm:?}");
        }
    }

    let _ = writeln!(out, "\ntool mask (#116): {}", mask_summary(p));

    let _ = writeln!(out, "\nspawn control (#119):");
    let _ = writeln!(out, "  may_spawn: {}", p.may_spawn());
    match &p.spawnable_agents {
        Some(list) if list.is_empty() => {
            let _ = writeln!(out, "  spawnable_agents: [] (none)");
        }
        Some(list) => {
            let _ = writeln!(out, "  spawnable_agents: [{}]", list.join(","));
        }
        None => {
            let _ = writeln!(out, "  spawnable_agents: any spawnable target");
        }
    }

    let _ = writeln!(out, "\nowns_plan (#140): {}", p.owns_plan);
    let _ = writeln!(
        out,
        "\nassembled system prompt: {} chars",
        p.system_prompt.len()
    );
    out
}
