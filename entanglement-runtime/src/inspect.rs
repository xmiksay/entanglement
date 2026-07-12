//! `skutter inspect prompt` / `skutter inspect agents` — surface resolved runtime
//! state without spawning the engine (#184, #185).
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
use crate::skills;
use crate::system_prompt::PromptContext;

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

    let rows: Vec<[String; 6]> = resolved
        .iter()
        .map(|r| {
            [
                r.profile.name.clone(),
                format!("{:?}", r.profile.mode).to_lowercase(),
                r.profile.model.clone().unwrap_or_else(|| "inherit".into()),
                r.layer.label().to_string(),
                r.source.clone(),
                mask_summary(&r.profile),
            ]
        })
        .collect();

    let headers = ["NAME", "MODE", "MODEL", "LAYER", "SOURCE", "MASK"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    print_row(&headers.map(String::from), &widths);
    for row in &rows {
        print_row(row, &widths);
    }
}

/// Print one padded row; the last column is left un-padded (no trailing spaces).
fn print_row(cells: &[String; 6], widths: &[usize; 6]) {
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
