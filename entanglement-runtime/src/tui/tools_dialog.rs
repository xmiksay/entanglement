//! The `/agent` picker's `e` tools-checklist dialog (#330): edit a highlighted
//! profile's `tools:` allowlist in-app. [`ToolsDialog`] owns the roster, the
//! per-tool checked state, and the cursor — a single-stage checklist modal,
//! following the `/key` dialog's dedicated-state-module pattern
//! ([`crate::tui::key_dialog`]). The actual write goes through
//! [`crate::agents::save_tools_override`]; this module is pure state.

use ratatui::widgets::ListState;

/// A tools-checklist modal over one profile's full advertised tool roster.
pub struct ToolsDialog {
    visible: bool,
    agent: String,
    tools: Vec<String>,
    checked: Vec<bool>,
    state: ListState,
}

impl ToolsDialog {
    pub fn new() -> Self {
        Self {
            visible: false,
            agent: String::new(),
            tools: Vec::new(),
            checked: Vec::new(),
            state: ListState::default(),
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    pub fn is_checked(&self, i: usize) -> bool {
        self.checked.get(i).copied().unwrap_or(false)
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    /// Open the checklist for `agent` over `roster`, seeding each checkbox from
    /// the profile's current effective mask: an omitted allowlist (`tools:
    /// None`) means every tool starts checked; otherwise checked = in the
    /// allowlist and not in the denylist — the same resolution
    /// [`entanglement_core::AgentProfile::advertises_tool`] applies.
    pub fn show(
        &mut self,
        agent: String,
        roster: Vec<String>,
        tools: Option<&[String]>,
        disallowed: &[String],
    ) {
        self.checked = roster
            .iter()
            .map(|t| {
                let allowed = tools
                    .map(|list| list.iter().any(|a| a == t))
                    .unwrap_or(true);
                allowed && !disallowed.iter().any(|d| d == t)
            })
            .collect();
        self.tools = roster;
        self.agent = agent;
        self.visible = true;
        self.state.select((!self.tools.is_empty()).then_some(0));
    }

    /// Close without saving — nothing is written.
    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn select_next(&mut self) {
        if self.tools.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        self.state.select(Some((current + 1) % self.tools.len()));
    }

    pub fn select_prev(&mut self) {
        if self.tools.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let prev = if current == 0 {
            self.tools.len() - 1
        } else {
            current - 1
        };
        self.state.select(Some(prev));
    }

    /// Space: flip the highlighted row's checkbox.
    pub fn toggle_selected(&mut self) {
        if let Some(i) = self.state.selected() {
            if let Some(c) = self.checked.get_mut(i) {
                *c = !*c;
            }
        }
    }

    /// Resolve the checked set to a `tools:` allowlist: every tool checked ⇒
    /// `None` (inherit all, matching the all-checked seed), else the explicit
    /// checked subset in roster order.
    pub fn to_allowlist(&self) -> Option<Vec<String>> {
        if self.checked.iter().all(|&c| c) {
            None
        } else {
            Some(
                self.tools
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, &c)| c)
                    .map(|(t, _)| t.clone())
                    .collect(),
            )
        }
    }
}

impl Default for ToolsDialog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<String> {
        vec!["read".into(), "edit".into(), "bash".into()]
    }

    #[test]
    fn omitted_allowlist_seeds_everything_checked() {
        let mut d = ToolsDialog::new();
        d.show("build".into(), roster(), None, &[]);
        assert!(d.visible());
        assert_eq!(d.agent(), "build");
        assert!((0..3).all(|i| d.is_checked(i)));
        assert_eq!(d.to_allowlist(), None);
    }

    #[test]
    fn explicit_allowlist_and_denylist_seed_checkboxes() {
        let mut d = ToolsDialog::new();
        d.show(
            "plan".into(),
            roster(),
            Some(&["read".to_string(), "edit".to_string()]),
            &["edit".to_string()],
        );
        assert!(d.is_checked(0), "read: allowlisted, not denied");
        assert!(!d.is_checked(1), "edit: allowlisted but denied");
        assert!(!d.is_checked(2), "bash: not allowlisted");
    }

    #[test]
    fn toggle_flips_the_highlighted_row() {
        let mut d = ToolsDialog::new();
        d.show("build".into(), roster(), None, &[]);
        d.toggle_selected();
        assert!(!d.is_checked(0));
        d.toggle_selected();
        assert!(d.is_checked(0));
    }

    #[test]
    fn navigation_wraps() {
        let mut d = ToolsDialog::new();
        d.show("build".into(), roster(), None, &[]);
        d.select_prev();
        assert_eq!(d.state().selected(), Some(2));
        d.select_next();
        assert_eq!(d.state().selected(), Some(0));
    }

    #[test]
    fn to_allowlist_collapses_all_checked_to_none() {
        let mut d = ToolsDialog::new();
        d.show("plan".into(), roster(), Some(&["read".to_string()]), &[]);
        assert_eq!(d.to_allowlist(), Some(vec!["read".to_string()]));
        // Check everything else too — back to inherit-all.
        d.state().select(Some(1));
        d.toggle_selected();
        d.state().select(Some(2));
        d.toggle_selected();
        assert_eq!(d.to_allowlist(), None);
    }

    #[test]
    fn to_allowlist_can_be_empty_deny_all() {
        let mut d = ToolsDialog::new();
        d.show("build".into(), roster(), None, &[]);
        for i in 0..3 {
            d.state().select(Some(i));
            d.toggle_selected();
        }
        assert_eq!(d.to_allowlist(), Some(vec![]));
    }

    #[test]
    fn hide_discards_without_saving() {
        let mut d = ToolsDialog::new();
        d.show("build".into(), roster(), None, &[]);
        d.toggle_selected();
        d.hide();
        assert!(!d.visible());
    }
}
