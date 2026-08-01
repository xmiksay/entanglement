//! `App` surface for modal/popup mouse hit-testing (Issue 1 — make everything
//! reasonable clickable). Each open modal captures its list `Rect` at draw time
//! via the `set_modal_*` setters here; `modal_events::handle_mouse` reads them
//! back through the `modal_*_rect` getters to map a left-click to a row index
//! and dispatch the same action the `Enter` key uses. Mirrors the existing
//! `set_chat_hit_test`/`block_at` capture-and-query pattern for transcript
//! clicks.

use ratatui::layout::Rect;

use super::App;

impl App {
    /// Record the input box's rect this frame — the slash/mention popup
    /// geometry anchors above the input, so reproducing it on click needs the
    /// same base rect the drawer used.
    pub fn set_input_area(&mut self, area: Rect) {
        self.input_area = area;
    }

    pub fn set_sessions_modal_rect(&mut self, area: Rect) {
        self.modal_click.sessions = area;
    }
    pub fn sessions_modal_rect(&self) -> Rect {
        self.modal_click.sessions
    }

    pub fn set_resume_modal_rect(&mut self, area: Rect) {
        self.modal_click.resume = area;
    }
    pub fn resume_modal_rect(&self) -> Rect {
        self.modal_click.resume
    }

    pub fn set_profile_picker_rect(&mut self, area: Rect) {
        self.modal_click.profile_picker = area;
    }
    pub fn profile_picker_rect(&self) -> Rect {
        self.modal_click.profile_picker
    }

    pub fn set_model_picker_rect(&mut self, area: Rect) {
        self.modal_click.model_picker = area;
    }
    pub fn model_picker_rect(&self) -> Rect {
        self.modal_click.model_picker
    }

    pub fn set_key_dialog_rect(&mut self, area: Rect) {
        self.modal_click.key_dialog = area;
    }
    pub fn key_dialog_rect(&self) -> Rect {
        self.modal_click.key_dialog
    }

    pub fn set_tools_dialog_rect(&mut self, area: Rect) {
        self.modal_click.tools_dialog = area;
    }
    pub fn tools_dialog_rect(&self) -> Rect {
        self.modal_click.tools_dialog
    }

    pub fn set_session_tools_dialog_rect(&mut self, area: Rect) {
        self.modal_click.session_tools_dialog = area;
    }
    pub fn session_tools_dialog_rect(&self) -> Rect {
        self.modal_click.session_tools_dialog
    }

    /// The command palette's list chunk (below the query row) — the `List`
    /// widget's own area, not the palette's outer frame.
    pub fn set_command_palette_rect(&mut self, area: Rect) {
        self.modal_click.command_palette_list = area;
    }
    pub fn command_palette_rect(&self) -> Rect {
        self.modal_click.command_palette_list
    }

    pub fn set_mcp_panel_rect(&mut self, area: Rect) {
        self.modal_click.mcp_panel = area;
    }
    pub fn mcp_panel_rect(&self) -> Rect {
        self.modal_click.mcp_panel
    }

    pub fn set_slash_popup_rect(&mut self, area: Rect) {
        self.modal_click.slash_popup = area;
    }
    pub fn slash_popup_rect(&self) -> Rect {
        self.modal_click.slash_popup
    }

    pub fn set_mention_popup_rect(&mut self, area: Rect) {
        self.modal_click.mention_popup = area;
    }
    pub fn mention_popup_rect(&self) -> Rect {
        self.modal_click.mention_popup
    }
}
