use entanglement_core::AgentState;
use std::time::Instant;

use crate::tui::commands::CommandPalette;
use crate::tui::keybindings::LeaderKeyHandler;
use crate::tui::session_view::{ApprovalMode, PendingQuestion};
use crate::tui::theme::Theme;

use super::App;

impl App {
    pub fn approval_mode(&self) -> &ApprovalMode {
        self.sessions.active_view().approval_mode()
    }

    pub fn pending_tool_request(&self) -> Option<&(String, String, String)> {
        self.sessions.active_view().pending_tool_request()
    }

    pub fn queued_approvals(&self) -> usize {
        self.sessions.active_view().queued_approvals()
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.sessions.active_view_mut().set_approval_mode(mode);
        self.mark_dirty();
    }

    /// Resolve the prompted approval and surface the next queued one (#273).
    pub fn advance_approval(&mut self) {
        self.sessions.active_view_mut().advance_approval();
        self.mark_dirty();
    }

    pub fn clear_approval(&mut self) {
        self.sessions.active_view_mut().clear_approval();
        self.mark_dirty();
    }

    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.sessions.active_view().pending_question()
    }

    pub fn is_asking(&self) -> bool {
        self.sessions.active_view().is_asking()
    }

    pub fn question_move(&mut self, delta: isize) {
        self.sessions.active_view_mut().question_move(delta);
        self.mark_dirty();
    }

    /// Page the highlighted choice by `delta`, clamping at the bounds (no
    /// wrap) — used by `PageUp`/`PageDown` in the `ask_user` question dialog.
    pub fn question_page(&mut self, delta: isize) {
        self.sessions.active_view_mut().question_page(delta);
        self.mark_dirty();
    }

    pub fn question_toggle(&mut self) {
        self.sessions.active_view_mut().question_toggle();
        self.mark_dirty();
    }

    pub fn question_toggle_at(&mut self, idx: usize) {
        self.sessions.active_view_mut().question_toggle_at(idx);
        self.mark_dirty();
    }

    /// Records the current question's answer as a revisable **draft** and
    /// steps forward — to the next question, or to the review/submit step
    /// once every question in the call has a draft (#518: nothing is sent
    /// here; see [`Self::question_answers_for_submit`]). Reloads the shared
    /// input box when landing on a question last answered free-form.
    pub fn commit_question_answer(&mut self, answer: Vec<String>) {
        let restore = self
            .sessions
            .active_view_mut()
            .commit_question_answer(answer);
        self.load_question_input(restore);
        self.mark_dirty();
    }

    /// Steps back to the previous question, restoring its draft — or leaves
    /// the review/submit step to revise the last question (#518). A no-op on
    /// the first question.
    pub fn question_retreat(&mut self) {
        let restore = self.sessions.active_view_mut().question_retreat();
        self.load_question_input(restore);
        self.mark_dirty();
    }

    /// The final per-question answers, ready to send as one `AnswerQuestion`,
    /// once the front call's review/submit step has been reached (#518).
    /// `None` otherwise — never sent implicitly.
    pub fn question_answers_for_submit(&self) -> Option<Vec<Vec<String>>> {
        self.sessions.active_view().question_answers_for_submit()
    }

    /// Loads `restore` into the shared input box (a free-text draft reloaded
    /// while stepping between questions), or clears it when `None`. Uses
    /// [`App::set_input_text`] rather than [`App::take_input_text`] so this
    /// internal housekeeping never pollutes the user's input history.
    fn load_question_input(&mut self, restore: Option<String>) {
        self.set_input_text(restore.unwrap_or_default());
    }

    pub fn question_begin_free_form(&mut self) {
        self.sessions.active_view_mut().question_begin_free_form();
        self.mark_dirty();
    }

    pub fn question_cancel_free_form(&mut self) {
        self.sessions.active_view_mut().question_cancel_free_form();
        self.mark_dirty();
    }

    /// Resolve the prompted question and surface the next queued one (#273).
    pub fn advance_question(&mut self) {
        self.sessions.active_view_mut().advance_question();
        self.mark_dirty();
    }

    pub fn clear_question(&mut self) {
        self.sessions.active_view_mut().clear_question();
        self.mark_dirty();
    }

    /// Switches the active view to `id` (a sidebar click). A no-op for an
    /// unknown id — the registry ignores it.
    pub fn switch_to_session(&mut self, id: entanglement_core::SessionId) {
        self.sessions.switch_to(id);
        self.mark_dirty();
    }

    /// Jumps to the oldest background session parked on an approval or
    /// question (`Ctrl+G` / a click on the attention panel): switching makes
    /// it the active view, so the existing approval/question UI takes over.
    /// Returns whether a jump happened.
    pub fn jump_to_next_attention(&mut self) -> bool {
        let active = self.sessions.active_id().clone();
        let target = self
            .sessions
            .all()
            .into_iter()
            .find(|(id, view)| **id != active && crate::tui::format::attention_word(view).is_some())
            .map(|(id, _)| id.clone());
        match target {
            Some(id) => {
                self.sessions.switch_to(id);
                self.mark_dirty();
                true
            }
            None => false,
        }
    }

    pub fn leader_handler(&mut self) -> &mut LeaderKeyHandler {
        &mut self.leader_handler
    }

    /// Immutable view of the leader-key handler's keymap, for read-only
    /// rendering (the help popup) that also needs to mutate other `App`
    /// state in the same call.
    pub fn keymap(&self) -> &crate::tui::keybindings::KeyMap {
        self.leader_handler.keymap()
    }

    pub fn showing_help(&self) -> bool {
        self.showing_help
    }

    pub fn help_scroll(&self) -> u16 {
        self.help_scroll
    }

    /// Set the Help popup scroll offset (clamped at 0; the event loop clamps
    /// the upper bound against the rendered line count).
    pub fn set_help_scroll(&mut self, scroll: u16) {
        self.help_scroll = scroll;
        self.mark_dirty();
    }

    pub fn toggle_help(&mut self) {
        self.showing_help = !self.showing_help;
        self.help_scroll = 0;
        self.mark_dirty();
    }

    pub fn close_help(&mut self) {
        self.showing_help = false;
        self.help_scroll = 0;
        self.mark_dirty();
    }

    pub fn showing_command_palette(&self) -> bool {
        self.command_palette.visible()
    }

    pub fn toggle_command_palette(&mut self) {
        if self.command_palette.visible() {
            self.command_palette.hide();
        } else {
            self.command_palette.show();
        }
        self.mark_dirty();
    }

    pub fn close_command_palette(&mut self) {
        self.command_palette.hide();
        self.mark_dirty();
    }

    pub fn command_palette(&mut self) -> &mut CommandPalette {
        &mut self.command_palette
    }

    pub fn showing_sidebar(&self) -> bool {
        self.sidebar_visible
    }

    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        self.mark_dirty();
    }

    /// Reveals the sidebar (idempotent) — backs `/tasks`, which jumps to the
    /// sidebar's Tasks section (#325). `/plan` no longer uses this: it opens
    /// the bound plan file in `$EDITOR` instead (#513).
    pub fn show_sidebar(&mut self) {
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.mark_dirty();
        }
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn profile_color_for(&self, name: &str) -> ratatui::style::Color {
        self.profile_colors
            .get(name)
            .copied()
            .unwrap_or_else(|| crate::tui::theme::hash_profile_color(name))
    }

    pub fn thinking_since(&self) -> Option<Instant> {
        self.thinking_since
    }

    pub fn tick_thinking(&mut self) {
        // The ship-cruise animation spins while the session is busy — generating
        // (`Thinking`) or executing tools (`Working`) — but not while parked on
        // an approval/question/sub-agent (ADR-0139).
        let is_thinking = matches!(self.state(), AgentState::Thinking | AgentState::Working);
        match (self.thinking_since, is_thinking) {
            (None, true) => {
                self.thinking_since = Some(Instant::now());
                self.mark_dirty();
            }
            (Some(_), false) => {
                self.thinking_since = None;
                self.mark_dirty();
            }
            (Some(_), true) => {
                self.mark_dirty();
            }
            _ => {}
        }
    }

    pub fn input_tokens(&self) -> u64 {
        self.sessions.active_view().input_tokens()
    }

    pub fn output_tokens(&self) -> u64 {
        self.sessions.active_view().output_tokens()
    }

    /// Accumulated session cost in USD (#192), summed from `OutEvent::Usage`.
    pub fn cost_usd(&self) -> f64 {
        self.sessions.active_view().cost_usd()
    }

    #[allow(dead_code)]
    pub fn is_input_multiline(&self) -> bool {
        self.input_multiline
    }

    #[allow(dead_code)]
    pub fn toggle_input_multiline(&mut self) {
        self.input_multiline = !self.input_multiline;
        self.mark_dirty();
    }

    pub fn set_input_multiline(&mut self, multiline: bool) {
        self.input_multiline = multiline;
        self.mark_dirty();
    }
}
