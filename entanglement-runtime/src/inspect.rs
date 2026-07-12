//! `skutter inspect prompt` / `skutter inspect agents` / `skutter inspect skills`
//! — surface resolved runtime state without spawning the engine (#184–#186).
//!
//! `inspect skills` (#186) closes the skill-authoring blind spot: skill selection
//! is "description quality is the contract," yet the author could not see the
//! exact tier-1 `disclosures()` block the model gets, whether `user_only` withheld
//! a skill, which layer won a collision, or whether a `${SKILL_DIR}` payload path
//! resolves. No `name` prints a table (name, user_only, layer, root_dir,
//! description); `--disclosures` prints the exact block the model receives; a
//! `name` dry-runs the `load_skill` path substitution so those bugs surface
//! without starting a session and asking the model.
//!
//! `inspect agents` closes the layer-collision blind spot (#185): three layers
//! (built-in < user < project) merge by `name` with a silent later-wins
//! `insert`, so a user could not tell whether their `~/.config` `build.md`
//! actually won, nor what the final permission/mask/mode is. It reuses the same
//! discovery as startup, keeps the provenance the `insert` swallows
//! ([`agents::resolve_registry`]), and prints a table or a per-agent detail view.
//!
//! `inspect prompt` (#184) —
//! The prompt each agent ships (preamble + body + brief + env + skill index +
//! preloaded skills) is baked into `AgentProfile.system_prompt` at load and was
//! observable nowhere. This subcommand runs the *same* discovery
//! ([`crate::system_prompt::PromptContext::load`] + skill/agent registries) that
//! startup does, **without spawning the engine**, and prints the resolved prompt.
//! `--parts` additionally breaks it into its component slices, each annotated
//! with the source it came from — surfacing a wrong brief pick, an empty preamble
//! override, or a subagent silently losing the skill index before model behaviour
//! degrades.
//!
//! The `render_*` helpers all return a `String` rather than printing directly, so
//! the TUI in-session inspection overlay (#214) renders the exact same views the
//! CLI does — see [`tui_reports`].

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::AgentProfile;

use crate::agents::{self, AgentLayer, AgentResolution};
use crate::skills::{self, SkillResolution};
use crate::system_prompt::{self, PromptContext};

/// Load registries for `cwd` (no engine), resolve `agent`, and print its
/// assembled system prompt — or, with `parts`, the per-slice breakdown.
pub fn inspect_prompt(cwd: &Path, agent: &str, parts: bool) -> Result<()> {
    let skill_registry = skills::load_registry(cwd).context("loading skill definitions")?;
    let mut ctx = PromptContext::load(cwd);
    ctx.skills = skill_registry.disclosures();

    let report = agents::prompt_report(cwd, agent, &ctx, &skill_registry)
        .context("assembling agent prompt")?
        .with_context(|| format!("unknown agent `{agent}` (no matching definition found)"))?;

    if parts {
        print!("{}", render_prompt_parts(agent, &ctx, &report));
    } else {
        println!("{}", report.profile.system_prompt);
    }
    Ok(())
}

/// Render the component breakdown: a header, then each included slice preceded by
/// a `── <label> ──  (source: …)` divider, and a trailing note when the brief was
/// requested but not folded in (or not requested at all).
fn render_prompt_parts(
    agent: &str,
    ctx: &PromptContext,
    report: &agents::AgentPromptReport,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "agent:  {agent}");
    let _ = writeln!(out, "source: {}", report.source);
    let _ = writeln!(out, "mode:   {:?}", report.profile.mode);
    let _ = writeln!(
        out,
        "assembled: {} chars across {} part(s)\n",
        report.profile.system_prompt.len(),
        report.parts.len()
    );

    for part in &report.parts {
        let _ = writeln!(out, "── {} ──  (source: {})", part.label, part.source);
        let _ = writeln!(out, "{}\n", part.content);
    }

    // The motivating failure (#184): a wrong/absent brief is invisible. Call it
    // out explicitly when the brief did not make it into the prompt.
    if !report.brief_included {
        let why = if !report.include_brief {
            "include_brief is not set on this agent".to_string()
        } else {
            match &ctx.brief_path {
                Some(p) => format!("brief file {} read empty", p.display()),
                None => "no brief file was found (AGENTS.md / .claude/CLAUDE.md / …)".to_string(),
            }
        };
        let _ = writeln!(out, "note: project brief not included — {why}");
    }
    out
}

/// `skutter inspect agents [name]` (#185): surface the layer-collision winner the
/// silent `insert` used to swallow. With no `name`, print a table of every
/// resolved agent (name, mode, model, layer, source, mask). With a `name`, print
/// the full resolved profile — permission rules, tool mask, spawn control, plan
/// authority, prompt length — plus which lower-layer definitions it overrode.
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
fn render_agent_table(resolved: &[AgentResolution]) -> String {
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

    render_table(&["NAME", "MODE", "MODEL", "LAYER", "SOURCE", "MASK"], &rows)
}

/// Render a width-fit column table: pad every column to the widest cell (header
/// included) except the last, which is left un-padded (no trailing spaces).
fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let header_row: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    render_row(&mut out, &header_row, &widths);
    for row in rows {
        render_row(&mut out, row, &widths);
    }
    out
}

/// Append one padded row; the last column is left un-padded (no trailing spaces).
fn render_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let last = cells.len() - 1;
    let line: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i == last {
                c.clone()
            } else {
                format!("{:<width$}", c, width = widths[i])
            }
        })
        .collect();
    let _ = writeln!(out, "{}", line.join("  "));
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
fn render_agent_detail(entry: &AgentResolution) -> String {
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

/// `skutter inspect skills [name] [--disclosures]` (#186). Three views over the
/// engine-free skill registry:
/// - `--disclosures`: the exact tier-1 block the model receives (takes priority);
/// - a `name`: a dry-run of the `load_skill` path substitution for that skill;
/// - neither: a table of every resolved skill with its winning layer + provenance.
pub fn inspect_skills(cwd: &Path, name: Option<&str>, disclosures: bool) -> Result<()> {
    if disclosures {
        let registry = skills::load_registry(cwd).context("loading skill definitions")?;
        print!("{}", render_disclosures(&registry.disclosures()));
        return Ok(());
    }

    let resolved = skills::resolve_registry(cwd).context("resolving skill registry")?;
    match name {
        Some(name) => {
            // Resolve with provenance so the dry-run shows the layer winner (the
            // same `SkillMeta` `load_skill` would pick) plus what it overrode.
            let entry = resolved
                .iter()
                .find(|r| r.meta.name == name)
                .with_context(|| {
                    format!("unknown skill `{name}` (no matching SKILL.md found in any layer)")
                })?;
            print!("{}", render_skill_dry_run(entry));
        }
        None => print!("{}", render_skill_table(&resolved)),
    }
    Ok(())
}

/// Render the exact tier-1 disclosure block the model is handed — the same
/// [`system_prompt::render_skills`] output the assembled prompt embeds, so what
/// the author sees here is byte-for-byte what drives selection.
fn render_disclosures(disclosures: &[crate::system_prompt::SkillDisclosure]) -> String {
    if disclosures.is_empty() {
        return "(no skills disclosed to the model — none discovered, or all are user_only)\n"
            .to_string();
    }
    format!("{}\n", system_prompt::render_skills(disclosures))
}

/// One-line-per-skill table: name, whether it is `user_only` (withheld from the
/// model), winning layer, its `root_dir`, and the description that drives
/// selection. Includes `user_only` skills — the author must see what was withheld.
fn render_skill_table(resolved: &[SkillResolution]) -> String {
    if resolved.is_empty() {
        return "no skill definitions found\n".to_string();
    }

    let rows: Vec<Vec<String>> = resolved
        .iter()
        .map(|r| {
            vec![
                r.meta.name.clone(),
                if r.meta.user_only { "yes" } else { "no" }.to_string(),
                r.layer.label().to_string(),
                r.meta
                    .root_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(built-in)".into()),
                r.meta.description.clone(),
            ]
        })
        .collect();

    render_table(
        &["NAME", "USER_ONLY", "LAYER", "ROOT_DIR", "DESCRIPTION"],
        &rows,
    )
}

/// Dry-run the `load_skill` resolution for one skill without a model: identity +
/// layer provenance (winner + what it overrode), then the exact `load_skill`
/// output (path-substituted body + `available_refs`). A `${SKILL_DIR}` or
/// relative-path bug surfaces right here.
fn render_skill_dry_run(entry: &SkillResolution) -> String {
    let skill = &entry.meta;
    let mut out = String::new();
    let _ = writeln!(out, "name:        {}", skill.name);
    let _ = writeln!(out, "description: {}", skill.description);
    let _ = writeln!(out, "user_only:   {}", skill.user_only);
    let _ = writeln!(out, "layer:       {}", entry.layer.label());
    let _ = writeln!(out, "source:      {}", entry.source);
    match &skill.root_dir {
        Some(dir) => {
            let _ = writeln!(out, "root_dir:    {}", dir.display());
        }
        None => {
            let _ = writeln!(
                out,
                "root_dir:    (built-in, single-file — no payload paths)"
            );
        }
    }

    if entry.shadowed.is_empty() {
        let _ = writeln!(out, "overrides:   (none — no lower-layer definition)");
    } else {
        let _ = writeln!(out, "overrides:");
        for (layer, source) in &entry.shadowed {
            let _ = writeln!(out, "  - {} ({source})", layer.label());
        }
    }

    if skill.user_only {
        // `load_skill` refuses a `user_only` skill to a model; this authoring
        // dry-run renders it anyway — call out that the model never sees it.
        let _ = writeln!(
            out,
            "\nnote: user_only — the model can never load this via `load_skill`; \
             shown here for authoring only."
        );
    }

    let _ = writeln!(
        out,
        "\n─── load_skill output (path substitution applied) ───\n"
    );
    let _ = writeln!(out, "{}", skills::load_skill::render_skill(skill));
    out
}

/// The three rendered inspection views for the TUI overlay (#214): the assembled
/// prompt breakdown, the agent registry, and the skill registry — each a
/// self-contained `String` ready to drop into a scrollable pane.
pub(crate) struct InspectReports {
    pub prompt: String,
    pub agents: String,
    pub skills: String,
}

/// Resolve and render the three inspection views for the in-session TUI overlay
/// (#214), reusing the exact `render_*` helpers the CLI prints. `agent` is the
/// **active session's** resolved agent — the overlay inspects the live session's
/// state rather than taking `--agent`. Resolution errors are rendered inline (as
/// `error: …`) so the overlay always has something to show rather than failing to
/// open.
pub(crate) fn tui_reports(cwd: &Path, agent: &str) -> InspectReports {
    InspectReports {
        prompt: tui_prompt(cwd, agent),
        agents: tui_agents(cwd, agent),
        skills: tui_skills(cwd),
    }
}

/// The active agent's assembled prompt, always broken into parts (the `--parts`
/// view) — the breakdown is what makes a wrong brief/preamble pick visible.
fn tui_prompt(cwd: &Path, agent: &str) -> String {
    let build = || -> Result<String> {
        let skill_registry = skills::load_registry(cwd).context("loading skill definitions")?;
        let mut ctx = PromptContext::load(cwd);
        ctx.skills = skill_registry.disclosures();
        let report = agents::prompt_report(cwd, agent, &ctx, &skill_registry)
            .context("assembling agent prompt")?
            .with_context(|| format!("unknown agent `{agent}` (no matching definition found)"))?;
        Ok(render_prompt_parts(agent, &ctx, &report))
    };
    build().unwrap_or_else(|e| format!("error: {e:#}"))
}

/// The resolved agent registry table, followed by the full detail for the
/// **active** agent so the "why was this denied / which layer won" state is one
/// glance away for the live session.
fn tui_agents(cwd: &Path, agent: &str) -> String {
    let build = || -> Result<String> {
        let skill_registry = skills::load_registry(cwd).context("loading skill definitions")?;
        let mut ctx = PromptContext::load(cwd);
        ctx.skills = skill_registry.disclosures();
        let resolved = agents::resolve_registry(cwd, &ctx, &skill_registry)
            .context("resolving agent registry")?;
        let mut out = render_agent_table(&resolved);
        if let Some(entry) = resolved.iter().find(|r| r.profile.name == agent) {
            out.push('\n');
            let _ = writeln!(out, "═══ active agent: {agent} ═══\n");
            out.push_str(&render_agent_detail(entry));
        }
        Ok(out)
    };
    build().unwrap_or_else(|e| format!("error: {e:#}"))
}

/// The exact disclosure block the model sees, then the full skill table
/// (including `user_only` skills the model never receives).
fn tui_skills(cwd: &Path) -> String {
    let build = || -> Result<String> {
        let registry = skills::load_registry(cwd).context("loading skill definitions")?;
        let resolved = skills::resolve_registry(cwd).context("resolving skill registry")?;
        let mut out = String::new();
        let _ = writeln!(out, "═══ disclosures (what the model sees) ═══\n");
        out.push_str(&render_disclosures(&registry.disclosures()));
        let _ = writeln!(out, "\n═══ all skills (including user_only) ═══\n");
        out.push_str(&render_skill_table(&resolved));
        Ok(out)
    };
    build().unwrap_or_else(|e| format!("error: {e:#}"))
}
