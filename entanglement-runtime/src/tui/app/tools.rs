//! `App` surface for the `/agent` picker's `e` tools-checklist dialog (#330):
//! open/close, navigation, and the submit path that materializes the checked
//! set as a user-layer override via [`crate::agents::save_tools_override`] and
//! records a transcript status line (the write takes effect on the next
//! restart — there is no live watcher yet, ADR-0081-style follow-up).

use ratatui::widgets::ListState;

use crate::tui::tools_dialog::ToolsDialog;

use super::App;

/// Outcome of submitting the tools dialog.
pub enum ToolsSubmit {
    /// The override was written; the dialog closed.
    Saved,
    /// The write failed; the dialog stays open for a retry.
    Failed,
    /// Nothing to submit (no profile was highlighted when the dialog opened).
    Noop,
}

impl App {
    pub fn showing_tools_dialog(&self) -> bool {
        self.tools_dialog.visible()
    }

    pub fn tools_dialog(&self) -> &ToolsDialog {
        &self.tools_dialog
    }

    pub fn tools_dialog_state(&mut self) -> &mut ListState {
        self.tools_dialog.state()
    }

    /// Open the checklist for the profile currently highlighted in the
    /// `/agent` picker — not necessarily the active session's agent.
    pub fn open_tools_dialog(&mut self) {
        let Some(idx) = self.profile_picker_state.selected() else {
            return;
        };
        let Some(profile) = self.available_profiles.get(idx) else {
            return;
        };
        let agent = profile.name.clone();
        let tools = profile.tools.clone();
        let disallowed = profile.disallowed_tools.clone();
        self.tools_dialog.show(
            agent,
            self.tool_roster.clone(),
            tools.as_deref(),
            &disallowed,
        );
        self.mark_dirty();
    }

    pub fn close_tools_dialog(&mut self) {
        self.tools_dialog.hide();
        self.mark_dirty();
    }

    pub fn tools_dialog_next(&mut self) {
        self.tools_dialog.select_next();
        self.mark_dirty();
    }

    pub fn tools_dialog_prev(&mut self) {
        self.tools_dialog.select_prev();
        self.mark_dirty();
    }

    pub fn tools_dialog_toggle(&mut self) {
        self.tools_dialog.toggle_selected();
        self.mark_dirty();
    }

    /// Materialize the checked set as a user-layer override (#330) and record a
    /// transcript status line. Never touches the live engine — the new mask
    /// applies on the next restart.
    pub fn submit_tools_dialog(&mut self) -> ToolsSubmit {
        if !self.tools_dialog.visible() {
            return ToolsSubmit::Noop;
        }
        let agent = self.tools_dialog.agent().to_string();
        let allowed = self.tools_dialog.to_allowlist();
        match crate::agents::save_tools_override(&self.root, &agent, allowed.as_deref()) {
            Ok(path) => {
                self.tools_dialog.hide();
                self.sessions.active_view_mut().record_status(
                    "/agent",
                    format!(
                        "Saved tool allowlist for '{agent}' to {} (applies on next restart)",
                        path.display()
                    ),
                );
                self.mark_dirty();
                ToolsSubmit::Saved
            }
            Err(e) => {
                self.sessions.active_view_mut().record_status(
                    "/agent",
                    format!("Failed to save tool allowlist for '{agent}': {e:#}"),
                );
                self.mark_dirty();
                ToolsSubmit::Failed
            }
        }
    }
}
