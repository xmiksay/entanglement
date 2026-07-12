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
        print_parts(agent, &ctx, &report);
    } else {
        println!("{}", report.profile.system_prompt);
    }
    Ok(())
}

/// Render the component breakdown: a header, then each included slice preceded by
/// a `── <label> ──  (source: …)` divider, and a trailing note when the brief was
/// requested but not folded in (or not requested at all).
fn print_parts(agent: &str, ctx: &PromptContext, report: &agents::AgentPromptReport) {
    println!("agent:  {agent}");
    println!("source: {}", report.source);
    println!("mode:   {:?}", report.profile.mode);
    println!(
        "assembled: {} chars across {} part(s)\n",
        report.profile.system_prompt.len(),
        report.parts.len()
    );

    for part in &report.parts {
        println!("── {} ──  (source: {})", part.label, part.source);
        println!("{}\n", part.content);
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
        println!("note: project brief not included — {why}");
    }
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
            print_agent_detail(entry);
        }
        None => print_agent_table(&resolved),
    }
    Ok(())
}

/// One-line-per-agent table: name, mode, model, winning layer, source, and a
/// compact tool-mask summary. Columns are width-fit to the widest cell.
fn print_agent_table(resolved: &[AgentResolution]) {
    if resolved.is_empty() {
        println!("no agent definitions found");
        return;
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

    print_table(&["NAME", "MODE", "MODEL", "LAYER", "SOURCE", "MASK"], &rows);
}

/// Render a width-fit column table: pad every column to the widest cell (header
/// included) except the last, which is left un-padded (no trailing spaces).
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let header_row: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    print_row(&header_row, &widths);
    for row in rows {
        print_row(row, &widths);
    }
}

/// Print one padded row; the last column is left un-padded (no trailing spaces).
fn print_row(cells: &[String], widths: &[usize]) {
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
    println!("{}", line.join("  "));
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
fn print_agent_detail(entry: &AgentResolution) {
    let p = &entry.profile;
    println!("name:        {}", p.name);
    println!("description: {}", p.description);
    println!("mode:        {:?}", p.mode);
    println!(
        "model:       {}",
        p.model.as_deref().unwrap_or("inherit (session default)")
    );
    println!("layer:       {}", entry.layer.label());
    println!("source:      {}", entry.source);

    if entry.shadowed.is_empty() {
        println!("overrides:   (none — no lower-layer definition)");
    } else {
        println!("overrides:");
        for (layer, source) in &entry.shadowed {
            // The built-in `source` already reads `built-in (build.md)`; only a
            // file-path source needs its layer prefixed so it doesn't double up.
            if *layer == AgentLayer::BuiltIn {
                println!("  - {source}");
            } else {
                println!("  - {} ({source})", layer.label());
            }
        }
    }

    println!("\npermission (last matching rule wins):");
    println!("  default: {:?}", p.permission.default);
    if p.permission.rules.is_empty() {
        println!("  (no per-tool rules)");
    } else {
        for (pat, perm) in &p.permission.rules {
            println!("  {pat}: {perm:?}");
        }
    }

    println!("\ntool mask (#116): {}", mask_summary(p));

    println!("\nspawn control (#119):");
    println!("  may_spawn: {}", p.may_spawn());
    match &p.spawnable_agents {
        Some(list) if list.is_empty() => println!("  spawnable_agents: [] (none)"),
        Some(list) => println!("  spawnable_agents: [{}]", list.join(",")),
        None => println!("  spawnable_agents: any spawnable target"),
    }

    println!("\nowns_plan (#140): {}", p.owns_plan);
    println!("\nassembled system prompt: {} chars", p.system_prompt.len());
}

/// `skutter inspect skills [name] [--disclosures]` (#186). Three views over the
/// engine-free skill registry:
/// - `--disclosures`: the exact tier-1 block the model receives (takes priority);
/// - a `name`: a dry-run of the `load_skill` path substitution for that skill;
/// - neither: a table of every resolved skill with its winning layer + provenance.
pub fn inspect_skills(cwd: &Path, name: Option<&str>, disclosures: bool) -> Result<()> {
    if disclosures {
        let registry = skills::load_registry(cwd).context("loading skill definitions")?;
        print_disclosures(&registry.disclosures());
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
            print_skill_dry_run(entry);
        }
        None => print_skill_table(&resolved),
    }
    Ok(())
}

/// Print the exact tier-1 disclosure block the model is handed — the same
/// [`system_prompt::render_skills`] output the assembled prompt embeds, so what
/// the author sees here is byte-for-byte what drives selection.
fn print_disclosures(disclosures: &[crate::system_prompt::SkillDisclosure]) {
    if disclosures.is_empty() {
        println!("(no skills disclosed to the model — none discovered, or all are user_only)");
        return;
    }
    println!("{}", system_prompt::render_skills(disclosures));
}

/// One-line-per-skill table: name, whether it is `user_only` (withheld from the
/// model), winning layer, its `root_dir`, and the description that drives
/// selection. Includes `user_only` skills — the author must see what was withheld.
fn print_skill_table(resolved: &[SkillResolution]) {
    if resolved.is_empty() {
        println!("no skill definitions found");
        return;
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

    print_table(
        &["NAME", "USER_ONLY", "LAYER", "ROOT_DIR", "DESCRIPTION"],
        &rows,
    );
}

/// Dry-run the `load_skill` resolution for one skill without a model: identity +
/// layer provenance (winner + what it overrode), then the exact `load_skill`
/// output (path-substituted body + `available_refs`). A `${SKILL_DIR}` or
/// relative-path bug surfaces right here.
fn print_skill_dry_run(entry: &SkillResolution) {
    let skill = &entry.meta;
    println!("name:        {}", skill.name);
    println!("description: {}", skill.description);
    println!("user_only:   {}", skill.user_only);
    println!("layer:       {}", entry.layer.label());
    println!("source:      {}", entry.source);
    match &skill.root_dir {
        Some(dir) => println!("root_dir:    {}", dir.display()),
        None => println!("root_dir:    (built-in, single-file — no payload paths)"),
    }

    if entry.shadowed.is_empty() {
        println!("overrides:   (none — no lower-layer definition)");
    } else {
        println!("overrides:");
        for (layer, source) in &entry.shadowed {
            println!("  - {} ({source})", layer.label());
        }
    }

    if skill.user_only {
        // `load_skill` refuses a `user_only` skill to a model; this authoring
        // dry-run renders it anyway — call out that the model never sees it.
        println!(
            "\nnote: user_only — the model can never load this via `load_skill`; \
             shown here for authoring only."
        );
    }

    println!("\n─── load_skill output (path substitution applied) ───\n");
    println!("{}", skills::load_skill::render_skill(skill));
}
