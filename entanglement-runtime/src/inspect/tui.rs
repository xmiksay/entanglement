//! The in-session TUI inspection overlay's data source (#214).
//!
//! The `render_*` helpers the CLI subcommands use all return a `String` rather
//! than printing directly, so the TUI overlay renders the exact same views the
//! CLI does — resolved engine-lessly here, then dropped into scrollable panes by
//! `tui::app::inspect`.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::skills;
use crate::system_prompt::PromptContext;

use super::agents::{render_agent_detail, render_agent_table};
use super::prompt::render_prompt_parts;
use super::skills::{render_disclosures, render_skill_table};

/// The three rendered inspection views for the TUI overlay (#214): the assembled
/// prompt breakdown, the agent registry, and the skill registry — each a
/// self-contained `String` ready to drop into a scrollable pane.
pub struct InspectReports {
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
pub fn tui_reports(cwd: &Path, agent: &str) -> InspectReports {
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
        let report = crate::agents::prompt_report(cwd, agent, &ctx, &skill_registry)
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
        let resolved = crate::agents::resolve_registry(cwd, &ctx, &skill_registry)
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
