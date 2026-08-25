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
use crate::tool_state::{self, DispatchState, ToolState};

/// With no `name`, print a table of every resolved agent (name, mode, model,
/// layer, source, mask, dispatch tally). With a `name`, print the full resolved
/// profile — permission rules, tool mask, per-tool dispatch state, spawn
/// control, plan authority, prompt length — plus which lower-layer definitions
/// it overrode.
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

/// One-line-per-agent table: name, mode, model, winning layer, source, the
/// compact tool-mask summary (the *source* data the user edits) and the
/// three-state dispatch tally it produces. Columns are width-fit to the widest
/// cell.
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
                dispatch_tally(&r.profile),
            ]
        })
        .collect();

    super::render_table(
        &[
            "NAME", "MODE", "MODEL", "LAYER", "SOURCE", "MASK", "DISPATCH",
        ],
        &rows,
    )
}

/// What the mask + permission grades add up to, one cell wide: how many of the
/// built-in roster's tools each of the three states covers. The mask column
/// beside it stays the *source* data — this is its consequence, and the
/// per-tool breakdown is one `inspect agents <name>` away.
fn dispatch_tally(profile: &AgentProfile) -> String {
    let (mut allowed, mut asks, mut declines) = (0, 0, 0);
    for tool in tool_state::static_roster() {
        match tool_state::for_profile(profile, tool).state {
            DispatchState::Allowed => allowed += 1,
            DispatchState::Asks => asks += 1,
            DispatchState::Declines => declines += 1,
        }
    }
    format!("allowed:{allowed} asks:{asks} declines:{declines}")
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

/// The per-tool three-state view the mask + permission rules above produce.
/// WHY it is spelled out next to the mask rather than replacing it: the
/// frontmatter is what the user edits, the state is what the model gets — and
/// since advertisement stopped tracking the mask, a masked-out tool is *listed
/// here as declining*, never omitted, which is precisely the fact the old
/// presentation hid.
fn render_dispatch_states(profile: &AgentProfile) -> String {
    let states: Vec<(&str, ToolState)> = tool_state::static_roster()
        .into_iter()
        .map(|t| (t, tool_state::for_profile(profile, t)))
        .collect();
    let width = states.iter().map(|(t, _)| t.len()).max().unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "\ndispatch state (every tool is advertised; the mask and the grades \
         decide the outcome):"
    );
    for (tool, state) in &states {
        let _ = writeln!(out, "  {tool:<width$}  {}", state.label());
    }
    // MCP tools connect after the profiles load, so an engine-free view can't
    // name them — say so instead of implying this roster is exhaustive.
    let _ = writeln!(
        out,
        "  (built-in tools only — MCP tools resolve against a live session)"
    );
    out
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
    out.push_str(&render_dispatch_states(p));

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

    // Plan authorship is default-closed via explicit allowlist membership now
    // (#231, ADR-0049; #513, ADR-0145) — surfaced by the tool mask above, not a
    // dedicated flag.
    let authors_plan = crate::plan_tasks::explicitly_allowlists(p, "propose_plan");
    let _ = writeln!(out, "\nauthors plan (#231): {authors_plan}");
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
    use entanglement_core::{AgentMode, Permission, PermissionProfile};

    fn resolution(profile: AgentProfile) -> AgentResolution {
        AgentResolution {
            profile,
            layer: AgentLayer::BuiltIn,
            source: "built-in (t.md)".into(),
            shadowed: Vec::new(),
        }
    }

    fn profile(permission: PermissionProfile, tools: Option<Vec<&str>>) -> AgentProfile {
        AgentProfile {
            name: "t".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission,
            tools: tools.map(|v| v.into_iter().map(String::from).collect()),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        }
    }

    #[test]
    fn detail_lists_a_masked_out_tool_as_declining_never_omits_it() {
        // The old view presented the mask as presence/absence; every tool is
        // advertised now, so a withheld one must appear, marked.
        let detail = render_agent_detail(&resolution(profile(
            PermissionProfile::new(Permission::Allow),
            Some(vec!["read"]),
        )));
        assert!(
            detail.contains("tool mask (#116): allow:[read]"),
            "{detail}"
        );
        assert!(detail.contains("read          allowed"), "{detail}");
        assert!(detail.contains("edit          declines"), "{detail}");
    }

    #[test]
    fn detail_spells_out_an_argument_scoped_grade() {
        let permission = PermissionProfile::new(Permission::Ask)
            .with("write", Permission::Deny)
            .with("write(.entanglement/plans/*.md)", Permission::Allow);
        let detail = render_agent_detail(&resolution(profile(permission, None)));
        assert!(
            detail.contains("write         declines (allowed by argument)"),
            "{detail}"
        );
        // The mask stays visible as the source data the user actually edits.
        assert!(detail.contains("tool mask (#116): all"), "{detail}");
    }

    #[test]
    fn table_carries_the_mask_and_the_state_it_produces() {
        let rows = vec![resolution(profile(
            PermissionProfile::new(Permission::Ask),
            Some(vec!["read", "grep"]),
        ))];
        let table = render_agent_table(&rows);
        assert!(table.contains("MASK"), "{table}");
        assert!(table.contains("DISPATCH"), "{table}");
        assert!(table.contains("allow:[read,grep]"), "{table}");
        // Two `Ask` tools survive the mask; everything else in the roster is
        // withheld, so the tally must show both sides.
        assert!(table.contains("allowed:0 asks:2 declines:"), "{table}");
    }
}
