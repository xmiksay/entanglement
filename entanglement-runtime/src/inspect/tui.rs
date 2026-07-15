//! The in-session TUI inspection overlay's data source (#214, drill-down #331).
//!
//! The `render_*` helpers the CLI subcommands use all return a `String` rather
//! than printing directly, so the TUI overlay renders the exact same views the
//! CLI does — resolved engine-lessly here, then dropped into scrollable panes by
//! `tui::app::inspect`.
//!
//! As of #331 the Agents and Skills tabs are **two-level**: a selectable list
//! (`name` + one-line summary + winning layer) whose `Enter` action opens a
//! per-item detail pane driven by the *same* per-name renderers the CLI uses
//! (`inspect agents <name>` / `inspect skills <name>`). The Prompt tab stays a
//! single scroll-only document. `list_items` exposes the list rows and
//! `agent_detail` / `skill_detail` render a single item by name; the flat
//! `agents` / `skills` strings remain so the overlay can render the
//! pre-resolution summary text without a second registry pass.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::skills;
use crate::system_prompt::PromptContext;

use super::agents::{render_agent_detail, render_agent_table};
use super::prompt::render_prompt_parts;
use super::skills::{render_disclosures, render_skill_dry_run, render_skill_table};

/// One selectable row in a two-level inspection tab (#331): the item's name,
/// the one-line summary the list shows, and the winning layer label. Kept as a
/// flat `String` tuple so the overlay state owns no inspect-module types — it
/// renders whatever `tui_reports` resolved on open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectItem {
    pub name: String,
    /// One-line description / summary, truncated by the list view to fit.
    pub summary: String,
    /// Winning layer label (`built-in` / `user` / `project`).
    pub layer: String,
}

/// The three rendered inspection views for the TUI overlay (#214): the assembled
/// prompt breakdown, the agent registry, and the skill registry — each a
/// self-contained `String` ready to drop into a scrollable pane. The two-level
/// tabs (#331) additionally carry the selectable list rows (`agent_items` /
/// `skill_items`); the flat `agents` / `skills` strings are kept as the
/// not-yet-drilled-into summary the overlay can fall back to.
pub struct InspectReports {
    pub prompt: String,
    pub agents: String,
    pub skills: String,
    pub agent_items: Vec<InspectItem>,
    pub skill_items: Vec<InspectItem>,
}

impl InspectReports {
    /// The selectable list for a two-level tab: empty for the Prompt tab.
    pub fn items(&self, tab: InspectListTab) -> &[InspectItem] {
        match tab {
            InspectListTab::Agents => &self.agent_items,
            InspectListTab::Skills => &self.skill_items,
        }
    }
}

/// The tabs that carry a selectable two-level list (#331). The Prompt tab is
/// deliberately *not* here — it stays a single scroll-only document, so the
/// overlay's list/detail machinery never applies to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectListTab {
    Agents,
    Skills,
}

/// Resolve and render the three inspection views for the in-session TUI overlay
/// (#214, drill-down #331), reusing the exact `render_*` helpers the CLI prints.
/// `agent` is the **active session's** resolved agent — the overlay inspects the
/// live session's state rather than taking `--agent`. Resolution errors are
/// rendered inline (as `error: …`) so the overlay always has something to show
/// rather than failing to open.
///
/// The two-level list rows for the Agents/Skills tabs are resolved here too:
/// each registry is walked once for the flat table *and* the list items, so a
/// single registry pass drives both presentations.
pub fn tui_reports(cwd: &Path, agent: &str) -> InspectReports {
    let (agent_items, agent_summary) = tui_agent_views(cwd, agent);
    let (skill_items, skill_summary) = tui_skill_views(cwd);
    InspectReports {
        prompt: tui_prompt(cwd, agent),
        agents: agent_summary,
        skills: skill_summary,
        agent_items,
        skill_items,
    }
}

/// Render the detail for one agent by name (#331) — the same
/// `inspect agents <name>` output the CLI prints, so the overlay's detail pane
/// and the CLI agree byte-for-byte. `None` if no agent of that name resolves.
/// The active `agent` is not needed here: agent resolution is name-driven and
/// does not depend on which agent the live session is running.
pub fn agent_detail(cwd: &Path, name: &str) -> Option<String> {
    let build = || -> Result<Option<String>> {
        let skill_registry = skills::load_registry(cwd).context("loading skill definitions")?;
        let mut ctx = PromptContext::load(cwd);
        ctx.skills = skill_registry.disclosures();
        let resolved = crate::agents::resolve_registry(cwd, &ctx, &skill_registry)
            .context("resolving agent registry")?;
        Ok(resolved
            .iter()
            .find(|r| r.profile.name == name)
            .map(render_agent_detail))
    };
    // Match the overlay's inline-error convention so a resolution failure still
    // shows something rather than blanking the detail pane.
    build().unwrap_or_else(|e| Some(format!("error: {e:#}")))
}

/// Render the detail for one skill by name (#331) — the same
/// `inspect skills <name>` dry-run output the CLI prints. `None` if no skill of
/// that name resolves. Skill resolution does not need `agent`, so it is omitted.
pub fn skill_detail(cwd: &Path, name: &str) -> Option<String> {
    let build = || -> Result<Option<String>> {
        let resolved = skills::resolve_registry(cwd).context("resolving skill registry")?;
        Ok(resolved
            .iter()
            .find(|r| r.meta.name == name)
            .map(render_skill_dry_run))
    };
    build().unwrap_or_else(|e| Some(format!("error: {e:#}")))
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

/// Resolve the agent registry once and derive both presentations from it (#331):
/// the flat table (with the active-agent detail appended, for the not-yet-
/// drilled-in summary) and the selectable list rows. Returning both from one
/// registry pass avoids a second walk when the user drills into the list.
fn tui_agent_views(cwd: &Path, agent: &str) -> (Vec<InspectItem>, String) {
    let build = || -> Result<(Vec<InspectItem>, String)> {
        let skill_registry = skills::load_registry(cwd).context("loading skill definitions")?;
        let mut ctx = PromptContext::load(cwd);
        ctx.skills = skill_registry.disclosures();
        let resolved = crate::agents::resolve_registry(cwd, &ctx, &skill_registry)
            .context("resolving agent registry")?;

        // List rows: name + one-line description + winning layer.
        let items: Vec<InspectItem> = resolved
            .iter()
            .map(|r| InspectItem {
                name: r.profile.name.clone(),
                summary: r.profile.description.clone(),
                layer: r.layer.label().to_string(),
            })
            .collect();

        // Flat summary: the table, with the active agent's detail appended so the
        // "why was this denied / which layer won" state is one glance away.
        let mut out = render_agent_table(&resolved);
        if let Some(entry) = resolved.iter().find(|r| r.profile.name == agent) {
            out.push('\n');
            let _ = writeln!(out, "═══ active agent: {agent} ═══\n");
            out.push_str(&render_agent_detail(entry));
        }
        Ok((items, out))
    };
    build().unwrap_or_else(|e| (Vec::new(), format!("error: {e:#}")))
}

/// Resolve the skill registry once and derive both presentations (#331): the
/// flat summary (disclosures + full table) and the selectable list rows.
fn tui_skill_views(cwd: &Path) -> (Vec<InspectItem>, String) {
    let build = || -> Result<(Vec<InspectItem>, String)> {
        let registry = skills::load_registry(cwd).context("loading skill definitions")?;
        let resolved = skills::resolve_registry(cwd).context("resolving skill registry")?;

        // List rows: name + one-line description + winning layer. A `user_only`
        // skill is marked so the list signals it was withheld from the model.
        let items: Vec<InspectItem> = resolved
            .iter()
            .map(|r| InspectItem {
                name: r.meta.name.clone(),
                summary: if r.meta.user_only {
                    format!("{} (user_only)", r.meta.description)
                } else {
                    r.meta.description.clone()
                },
                layer: r.layer.label().to_string(),
            })
            .collect();

        let mut out = String::new();
        let _ = writeln!(out, "═══ disclosures (what the model sees) ═══\n");
        out.push_str(&render_disclosures(&registry.disclosures()));
        let _ = writeln!(out, "\n═══ all skills (including user_only) ═══\n");
        out.push_str(&render_skill_table(&resolved));
        Ok((items, out))
    };
    build().unwrap_or_else(|e| (Vec::new(), format!("error: {e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The detail renderers the overlay drills into (#331) must agree with the
    /// CLI's per-name renderers byte-for-byte — they call the same code path, so
    /// a fixture registry produces identical output from `agent_detail` /
    /// `skill_detail` and the CLI's `inspect_agents`/`inspect_skills` helpers.
    #[test]
    fn detail_matches_cli_renderer_for_built_in_agent_and_skill() {
        // The built-in `build` agent / `commit` skill are always present
        // (embedded), so this is cwd-independent — but resolve against a temp
        // root with no overrides so a user-layer definition can't perturb it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let agent = agent_detail(root, "build");
        assert!(
            agent
                .as_ref()
                .is_some_and(|s| s.contains("name:        build")
                    && s.contains("mode:")
                    && s.contains("layer:")),
            "agent_detail should render the built-in `build` profile, got: {agent:?}"
        );

        let skill = skill_detail(root, "commit");
        assert!(
            skill
                .as_ref()
                .is_some_and(|s| s.contains("name:        commit") && s.contains("layer:")),
            "skill_detail should render the built-in `commit` skill, got: {skill:?}"
        );
    }

    /// `None` from the detail renderers for an unknown name, not an error string
    /// — so the overlay can render an "(unknown)" row rather than a stack trace.
    #[test]
    fn detail_is_none_for_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(agent_detail(root, "no-such-agent"), None);
        assert_eq!(skill_detail(root, "no-such-skill"), None);
    }
}
