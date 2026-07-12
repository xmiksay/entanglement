//! `skutter inspect prompt` — show the assembled system prompt (#184).
//!
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

use crate::agents;
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
