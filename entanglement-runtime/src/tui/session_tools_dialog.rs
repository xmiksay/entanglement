//! Bare `/enable`'s session-tools checklist dialog (#539, ADR-0149): every
//! advertised tool with a checkbox reflecting the active session's *effective*
//! availability (profile mask overridden by the live tool overlay). Toggling
//! computes the overlay as a **diff against the profile**: a checked
//! not-profile-advertised tool becomes an enable entry, an unchecked
//! profile-advertised tool becomes a deny entry, and a row matching its
//! profile default contributes nothing — so `Enter` submits exactly the
//! overrides, and clearing every override empties the overlay. Follows the
//! `/agent` tools checklist's dedicated-state-module pattern
//! ([`crate::tui::tools_dialog`]); the actual write is
//! `InMsg::SetToolOverlay`, sent by the event handler.

use entanglement_core::ToolOverlayEntry;
use ratatui::widgets::ListState;

/// One roster tool's row: its profile-mask default, the current checkbox, and
/// the enable-grade flag (`allow` = skip the approval prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToolRow {
    pub name: String,
    /// Whether the active profile's own mask advertises this tool — the
    /// baseline a toggle diffs against.
    pub profile_default: bool,
    pub checked: bool,
    pub allow: bool,
}

/// The `/enable` checklist modal over the full advertised tool roster.
pub struct SessionToolsDialog {
    visible: bool,
    rows: Vec<SessionToolRow>,
    state: ListState,
}

impl SessionToolsDialog {
    pub fn new() -> Self {
        Self {
            visible: false,
            rows: Vec::new(),
            state: ListState::default(),
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn rows(&self) -> &[SessionToolRow] {
        &self.rows
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    /// Open the checklist over pre-seeded rows (the caller resolves each
    /// tool's profile default and overlay disposition — see
    /// `App::open_session_tools_dialog`).
    pub fn show(&mut self, rows: Vec<SessionToolRow>) {
        self.state.select((!rows.is_empty()).then_some(0));
        self.rows = rows;
        self.visible = true;
    }

    /// Close without saving — nothing is sent.
    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        self.state.select(Some((current + 1) % self.rows.len()));
    }

    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let prev = if current == 0 {
            self.rows.len() - 1
        } else {
            current - 1
        };
        self.state.select(Some(prev));
    }

    pub fn page_down(&mut self, n: usize) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        self.state
            .select(Some((current + n).min(self.rows.len() - 1)));
    }

    pub fn page_up(&mut self, n: usize) {
        let current = self.state.selected().unwrap_or(0);
        self.state.select(Some(current.saturating_sub(n)));
    }

    /// Flip the highlighted row's availability.
    pub fn toggle_selected(&mut self) {
        if let Some(row) = self.state.selected().and_then(|i| self.rows.get_mut(i)) {
            row.checked = !row.checked;
        }
    }

    /// Flip the highlighted row's auto-allow grade — meaningful only for a
    /// checked override row (an enable entry); a no-op elsewhere so the key
    /// can't create stray state.
    pub fn toggle_allow_selected(&mut self) {
        if let Some(row) = self.state.selected().and_then(|i| self.rows.get_mut(i)) {
            if row.checked && !row.profile_default {
                row.allow = !row.allow;
            }
        }
    }

    /// The overlay this checklist state materializes: the diff against each
    /// row's profile default. Patterns a hand-typed `/enable` added are thus
    /// expanded to the concrete currently-registered names — the same
    /// resolve-to-final-set behavior as the `/agent` tools dialog (#330).
    pub fn to_entries(&self) -> Vec<ToolOverlayEntry> {
        self.rows
            .iter()
            .filter_map(|row| match (row.checked, row.profile_default) {
                (true, false) => Some(ToolOverlayEntry {
                    pattern: row.name.clone(),
                    allow: row.allow,
                    deny: false,
                }),
                (false, true) => Some(ToolOverlayEntry::deny(row.name.clone())),
                _ => None,
            })
            .collect()
    }
}

impl Default for SessionToolsDialog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, profile_default: bool, checked: bool) -> SessionToolRow {
        SessionToolRow {
            name: name.into(),
            profile_default,
            checked,
            allow: false,
        }
    }

    #[test]
    fn to_entries_is_the_diff_against_the_profile() {
        let mut d = SessionToolsDialog::new();
        d.show(vec![
            row("read", true, true),                // default kept → no entry
            row("bash", true, true),                // will be unchecked → deny
            row("mcp__docs__search", false, false), // will be checked → enable
            row("mcp__jira__create", false, false), // untouched → no entry
        ]);
        d.state().select(Some(1));
        d.toggle_selected();
        d.state().select(Some(2));
        d.toggle_selected();
        d.toggle_allow_selected();
        assert_eq!(
            d.to_entries(),
            vec![
                ToolOverlayEntry::deny("bash"),
                ToolOverlayEntry {
                    pattern: "mcp__docs__search".into(),
                    allow: true,
                    deny: false,
                },
            ]
        );
    }

    #[test]
    fn matching_defaults_produce_an_empty_overlay() {
        let mut d = SessionToolsDialog::new();
        d.show(vec![
            row("read", true, true),
            row("mcp__x__y", false, false),
        ]);
        assert!(d.to_entries().is_empty());
    }

    #[test]
    fn allow_toggle_is_a_noop_on_profile_default_rows() {
        let mut d = SessionToolsDialog::new();
        d.show(vec![row("read", true, true)]);
        d.toggle_allow_selected();
        assert!(!d.rows()[0].allow, "profile-default rows carry no grade");
    }
}
