//! The `/agent` picker's `e` tools-checklist dialog (#330): edit a highlighted
//! profile's `tools:` allowlist in-app. [`ToolsDialog`] owns the roster, the
//! per-tool checked state, and the cursor — a single-stage checklist modal,
//! following the `/key` dialog's dedicated-state-module pattern
//! ([`crate::tui::key_dialog`]). The actual write goes through
//! [`crate::agents::save_tools_override`]; this module is pure state.
//!
//! The checkbox is the **mask** — the thing being edited — but a checked box no
//! longer means "the model can use this": advertisement stopped tracking the
//! mask, so every row is advertised and the mask only decides the dispatch
//! outcome. Each row therefore also carries its three-state
//! [`ToolState`][crate::tool_state::ToolState] (`allowed`/`asks`/`declines`,
//! recombined live with the checkbox), so unchecking a row shows the state it
//! will produce before the override is even saved.

use entanglement_core::{AgentProfile, PermissionProfile};
use ratatui::widgets::ListState;

use crate::tool_state::{self, ToolState};

/// A tools-checklist modal over one profile's full advertised tool roster.
pub struct ToolsDialog {
    visible: bool,
    agent: String,
    tools: Vec<String>,
    checked: Vec<bool>,
    /// Each row's dispatch state *assuming the mask admits it* — so a checkbox
    /// toggle recombines without re-resolving the permission profile.
    graded: Vec<ToolState>,
    state: ListState,
}

impl ToolsDialog {
    pub fn new() -> Self {
        Self {
            visible: false,
            agent: String::new(),
            tools: Vec::new(),
            checked: Vec::new(),
            graded: Vec::new(),
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

    /// What a dispatch of row `i` would do under the checklist's *current*
    /// state: an unchecked row is masked out, so it declines whatever its grade
    /// says; a checked one carries the grade the profile resolves for it.
    pub fn tool_state(&self, i: usize) -> ToolState {
        match self.graded.get(i) {
            Some(graded) if self.is_checked(i) => graded.clone(),
            _ => ToolState::declines(),
        }
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    /// Open the checklist for `agent` over `roster`, seeding each checkbox from
    /// the profile's current effective mask via [`AgentProfile::mask_allows`]
    /// itself — not a re-implementation — so a wildcard entry (#537, ADR-0148)
    /// seeds its matches checked exactly as the profile would grade them.
    /// Note the save path still emits the concrete checked set: submitting
    /// expands a glob to the currently registered names (hand-edit the
    /// frontmatter to keep a live pattern).
    ///
    /// `permission` + `may_spawn` seed the per-row dispatch state beside the
    /// checkbox. `may_spawn` is passed separately because it is the one gate on
    /// the `agent`/`agent_send` rows that checking the box cannot lift — the
    /// mask does not decide it.
    pub fn show(
        &mut self,
        agent: String,
        roster: Vec<String>,
        tools: Option<&[String]>,
        disallowed: &[String],
        permission: &PermissionProfile,
        may_spawn: bool,
    ) {
        self.checked = roster
            .iter()
            .map(|t| AgentProfile::mask_allows(tools, disallowed, t))
            .collect();
        self.graded = roster
            .iter()
            .map(|t| {
                if !may_spawn
                    && matches!(
                        t.as_str(),
                        crate::tool_names::AGENT_TOOL | crate::tool_names::AGENT_SEND_TOOL
                    )
                {
                    ToolState::declines()
                } else {
                    tool_state::graded(permission, t)
                }
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

    /// Page the highlight forward by `n`, clamping at the last tool. Unlike
    /// [`select_next`][Self::select_next] (which wraps), a page past the end
    /// stays on the last item.
    pub fn page_down(&mut self, n: usize) {
        if self.tools.is_empty() {
            return;
        }
        let last = self.tools.len() - 1;
        let current = self.state.selected().unwrap_or(0);
        self.state.select(Some((current + n).min(last)));
    }

    /// Page the highlight backward by `n`, clamping at the first tool.
    pub fn page_up(&mut self, n: usize) {
        if self.tools.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        self.state.select(Some(current.saturating_sub(n)));
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
    use entanglement_core::Permission;

    fn roster() -> Vec<String> {
        vec!["read".into(), "edit".into(), "bash".into()]
    }

    /// Open with a default-allow profile that may spawn — the mask, not the
    /// grade, is what most of these tests exercise.
    fn show(
        d: &mut ToolsDialog,
        agent: &str,
        roster: Vec<String>,
        tools: Option<&[String]>,
        disallowed: &[String],
    ) {
        let permission = PermissionProfile::new(Permission::Allow);
        d.show(agent.into(), roster, tools, disallowed, &permission, true);
    }

    #[test]
    fn omitted_allowlist_seeds_everything_checked() {
        let mut d = ToolsDialog::new();
        show(&mut d, "build", roster(), None, &[]);
        assert!(d.visible());
        assert_eq!(d.agent(), "build");
        assert!((0..3).all(|i| d.is_checked(i)));
        assert_eq!(d.to_allowlist(), None);
    }

    #[test]
    fn explicit_allowlist_and_denylist_seed_checkboxes() {
        let mut d = ToolsDialog::new();
        show(
            &mut d,
            "plan",
            roster(),
            Some(&["read".to_string(), "edit".to_string()]),
            &["edit".to_string()],
        );
        assert!(d.is_checked(0), "read: allowlisted, not denied");
        assert!(!d.is_checked(1), "edit: allowlisted but denied");
        assert!(!d.is_checked(2), "bash: not allowlisted");
    }

    #[test]
    fn glob_entry_seeds_its_matches_checked() {
        // #537: a wildcard mask entry seeds every matching roster tool exactly
        // as the engine would advertise it; saving still emits the concrete
        // checked set (a glob does not survive the round-trip).
        let mut d = ToolsDialog::new();
        let roster = vec![
            "read".to_string(),
            "mcp__docs__search".to_string(),
            "mcp__jira__create_issue".to_string(),
        ];
        show(
            &mut d,
            "custom",
            roster,
            Some(&["read".to_string(), "mcp__*".to_string()]),
            &["mcp__jira__*".to_string()],
        );
        assert!(d.is_checked(0), "read: literal entry");
        assert!(d.is_checked(1), "mcp__docs__search: matches mcp__*");
        assert!(!d.is_checked(2), "mcp__jira__create_issue: deny glob wins");
        assert_eq!(
            d.to_allowlist(),
            Some(vec!["read".to_string(), "mcp__docs__search".to_string()])
        );
    }

    #[test]
    fn toggle_flips_the_highlighted_row() {
        let mut d = ToolsDialog::new();
        show(&mut d, "build", roster(), None, &[]);
        d.toggle_selected();
        assert!(!d.is_checked(0));
        d.toggle_selected();
        assert!(d.is_checked(0));
    }

    #[test]
    fn navigation_wraps() {
        let mut d = ToolsDialog::new();
        show(&mut d, "build", roster(), None, &[]);
        d.select_prev();
        assert_eq!(d.state().selected(), Some(2));
        d.select_next();
        assert_eq!(d.state().selected(), Some(0));
    }

    #[test]
    fn to_allowlist_collapses_all_checked_to_none() {
        let mut d = ToolsDialog::new();
        show(&mut d, "plan", roster(), Some(&["read".to_string()]), &[]);
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
        show(&mut d, "build", roster(), None, &[]);
        for i in 0..3 {
            d.state().select(Some(i));
            d.toggle_selected();
        }
        assert_eq!(d.to_allowlist(), Some(vec![]));
    }

    #[test]
    fn hide_discards_without_saving() {
        let mut d = ToolsDialog::new();
        show(&mut d, "build", roster(), None, &[]);
        d.toggle_selected();
        d.hide();
        assert!(!d.visible());
    }

    #[test]
    fn page_down_clamps_at_last_tool() {
        let mut d = ToolsDialog::new();
        show(&mut d, "build", roster(), None, &[]);
        d.page_down(100);
        assert_eq!(d.state().selected(), Some(2));
    }

    #[test]
    fn page_up_clamps_at_first_tool() {
        let mut d = ToolsDialog::new();
        show(&mut d, "build", roster(), None, &[]);
        d.select_next(); // index 1
        d.page_up(100);
        assert_eq!(d.state().selected(), Some(0));
    }

    #[test]
    fn rows_carry_their_three_state_dispatch_outcome() {
        let mut d = ToolsDialog::new();
        let permission = PermissionProfile::new(Permission::Ask)
            .with("read", Permission::Allow)
            .with("edit", Permission::Deny)
            .with("edit(plans/*)", Permission::Allow);
        d.show("plan".into(), roster(), None, &[], &permission, true);
        assert_eq!(d.tool_state(0).label(), "allowed");
        // The argument-scoped escape hatch is spelled out, not rounded off.
        assert_eq!(d.tool_state(1).label(), "declines (allowed by argument)");
        assert_eq!(d.tool_state(2).label(), "asks");
    }

    #[test]
    fn unchecking_a_row_flips_its_state_to_declines_live() {
        // The mask decides only the dispatch outcome now — the tool stays
        // advertised — so the checklist must show the state the edit produces
        // before it is saved, rather than implying the row disappears.
        let mut d = ToolsDialog::new();
        show(&mut d, "build", roster(), None, &[]);
        assert_eq!(d.tool_state(0).label(), "allowed");
        d.toggle_selected();
        assert_eq!(d.tool_state(0).label(), "declines");
        d.toggle_selected();
        assert_eq!(d.tool_state(0).label(), "allowed");
    }

    #[test]
    fn spawn_rows_decline_for_a_profile_that_cannot_spawn() {
        // Checking the box can't lift `may_spawn` — the mask doesn't decide it.
        let mut d = ToolsDialog::new();
        let permission = PermissionProfile::new(Permission::Allow);
        let roster = vec!["read".to_string(), "agent".to_string()];
        d.show("leaf".into(), roster, None, &[], &permission, false);
        assert!(d.is_checked(1), "the mask still admits it");
        assert_eq!(d.tool_state(1).label(), "declines");
    }

    #[test]
    fn page_down_on_empty_roster_is_a_noop() {
        let mut d = ToolsDialog::new();
        show(&mut d, "build", vec![], None, &[]);
        d.page_down(8);
        assert_eq!(d.state().selected(), None);
    }
}
