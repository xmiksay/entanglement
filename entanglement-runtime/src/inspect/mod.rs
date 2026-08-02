//! `skutter inspect prompt` / `skutter inspect agents` / `skutter inspect skills`
//! — surface resolved runtime state without spawning the engine (#184–#186).
//!
//! Each subcommand runs the *same* load-time discovery that startup does
//! ([`crate::system_prompt::PromptContext::load`] + the skill/agent registries)
//! but **without spawning the engine**, so it is a cheap read-only fast path
//! (like `Sessions`). The three commands live in one submodule each — see their
//! module docs for the specific blind spot each closes:
//!
//! - [`prompt`] (#184): the assembled `AgentProfile.system_prompt`, or its
//!   per-slice `--parts` breakdown.
//! - [`agents`] (#185): the silent layer-collision winner + full resolved profile.
//! - [`skills`] (#186): the exact model-facing disclosures, plus a `load_skill`
//!   path-substitution dry-run.
//! - [`config`] (#172): the resolved user config with per-field provenance (which
//!   of default < user < project set each setting) and the permission ceiling.
//! - [`aux_models`] (#558): the persisted per-purpose auxiliary-LLM pins
//!   (`aux-models.yml`), otherwise only visible one purpose at a time via
//!   `/aux-model`.
//! - [`mcp_tokens`] (#558): which MCP servers hold a stored OAuth credential
//!   (`mcp-tokens.yml`, ADR-0153), redacted — never a token value.
//! - [`mcp`] (#558, deferred-work-ledger row 10c): the resolved bundled/
//!   available MCP server roster (provider-bundled + user `mcp:` entries),
//!   which servers auto-connect vs. wait for `/enable`.
//! - [`session`] (#558): a session's resolved live state folded straight from
//!   its persisted log — closes the one blind spot the four views above can't
//!   reach, the per-session tool overlay (ADR-0149), which has no managed file
//!   of its own.
//!
//! Every `render_*` helper returns a `String` rather than printing directly, so
//! the TUI in-session inspection overlay ([`tui`], #214) renders the exact same
//! views the CLI does.

mod agents;
mod aux_models;
mod config;
mod mcp;
mod mcp_tokens;
mod prompt;
mod session;
mod skills;
mod tui;

pub use agents::inspect_agents;
pub use aux_models::inspect_aux_models;
pub use config::inspect_config;
pub use mcp::inspect_mcp;
pub use mcp_tokens::inspect_mcp_tokens;
pub use prompt::inspect_prompt;
pub use session::inspect_session;
pub use skills::inspect_skills;
pub use tui::{
    agent_detail, skill_detail, tui_reports, InspectItem, InspectListTab, InspectReports,
};

use std::fmt::Write as _;

/// Render a width-fit column table: pad every column to the widest cell (header
/// included) except the last, which is left un-padded (no trailing spaces).
/// Shared by the `agents` and `skills` table views.
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
