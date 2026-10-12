//! The `/set` dialog's Tools tab: the per-session tool overlay — the rows
//! bare `/enable`'s checklist uses. (MCP servers need no enabling and the
//! advertised surface has no mode to re-pin since ADR-0226.)

use super::apply::{ApplyStep, ToolsChange};
use crate::tui::session_tools_dialog::{overlay_diff, SessionToolRow};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolsTab {
    rows: Vec<SessionToolRow>,
    initial: Vec<SessionToolRow>,
}

impl ToolsTab {
    pub fn new(rows: Vec<SessionToolRow>) -> Self {
        Self {
            initial: rows.clone(),
            rows,
        }
    }

    pub fn rows(&self) -> &[SessionToolRow] {
        &self.rows
    }

    pub fn toggle(&mut self, i: usize) {
        if let Some(row) = self.rows.get_mut(i) {
            row.checked = !row.checked;
        }
    }

    /// Auto-allow only means something on an enable override row — the same
    /// rule bare `/enable`'s checklist applies.
    pub fn toggle_allow(&mut self, i: usize) {
        if let Some(row) = self.rows.get_mut(i) {
            if row.checked && !row.profile_default {
                row.allow = !row.allow;
            }
        }
    }

    pub fn row_changed(&self, i: usize) -> bool {
        self.rows.get(i) != self.initial.get(i)
    }

    /// The overlay step, when the rows changed.
    pub fn overlay_step(&self) -> Option<ApplyStep> {
        (self.rows != self.initial).then(|| {
            ApplyStep::Tools(ToolsChange {
                entries: overlay_diff(&self.rows),
            })
        })
    }

    pub fn pending(&self) -> Vec<String> {
        self.rows
            .iter()
            .zip(&self.initial)
            .filter(|(now, was)| now != was)
            .map(|(now, _)| {
                let state = match (now.checked, now.allow) {
                    (false, _) => "off",
                    (true, true) => "on (allow)",
                    (true, false) => "on",
                };
                format!("tool {} → {state}", now.name)
            })
            .collect()
    }
}
