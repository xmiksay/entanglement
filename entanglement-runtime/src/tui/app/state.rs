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

    pub fn question_toggle(&mut self) {
        self.sessions.active_view_mut().question_toggle();
        self.mark_dirty();
    }

    pub fn question_toggle_at(&mut self, idx: usize) {
        self.sessions.active_view_mut().question_toggle_at(idx);
        self.mark_dirty();
    }

    /// Records the current question's answer; `Some` once the whole call is
    /// answered (#488).
    pub fn commit_question_answer(&mut self, answer: Vec<String>) -> Option<Vec<Vec<String>>> {
        let result = self
            .sessions
            .active_view_mut()
            .commit_question_answer(answer);
        self.mark_dirty();
        result
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

    pub fn leader_handler(&mut self) -> &mut LeaderKeyHandler {
        &mut self.leader_handler
    }

    pub fn showing_help(&self) -> bool {
        self.showing_help
    }

    pub fn toggle_help(&mut self) {
        self.showing_help = !self.showing_help;
        self.mark_dirty();
    }

    pub fn close_help(&mut self) {
        self.showing_help = false;
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

    /// Reveals the sidebar (idempotent) — backs `/plan` and `/tasks`, which
    /// jump to the sidebar's Plan Outline / Tasks sections (#325).
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
        let is_thinking = matches!(self.state(), AgentState::Thinking);
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
