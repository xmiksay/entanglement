use crate::tui::session_view::TranscriptEntry;
use crate::tui::theme::{RoleColors, Theme};
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::App;

impl App {
    /// Renders the active session's transcript body through its per-block render
    /// cache (#342). Splits `self` so the shared markdown renderer and the
    /// mutable active view are borrowed disjointly.
    pub(crate) fn render_cached_body(
        &mut self,
        width: u16,
        theme: Theme,
        user: RoleColors,
    ) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
        let Self {
            markdown_renderer,
            sessions,
            ..
        } = self;
        sessions
            .active_view_mut()
            .render_body(markdown_renderer, theme, user, width)
    }

    /// Blocks re-rendered on the active session's last body render (a #342 test
    /// hook; `0` when the redraw reused every cached block).
    #[cfg(test)]
    pub(crate) fn last_render_rebuilt(&self) -> usize {
        self.sessions.active_view().last_render_rebuilt()
    }

    pub fn scroll_offset(&self) -> usize {
        self.sessions.active_view().scroll_offset()
    }

    pub fn auto_follow(&self) -> bool {
        self.sessions.active_view().auto_follow()
    }

    /// Feeds the metrics `draw_body` measured back to the active session so the
    /// next scroll can clamp and follow can re-arm.
    pub fn set_viewport_metrics(&mut self, content_height: usize, viewport_height: usize) {
        self.sessions
            .active_view_mut()
            .set_viewport_metrics(content_height, viewport_height);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.sessions.active_view_mut().scroll_down(lines);
        self.mark_dirty();
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.sessions.active_view_mut().scroll_up(lines);
        self.mark_dirty();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.sessions.active_view_mut().scroll_to_bottom();
        self.mark_dirty();
    }

    /// Flips a collapsible block (reasoning run or tool op) between collapsed
    /// and expanded.
    pub fn toggle_block(&mut self, id: usize) {
        self.sessions.active_view_mut().toggle_block(id);
        self.mark_dirty();
    }

    /// Injects the shared HTTP transport so the bottom info line can surface a
    /// throttle indicator. Set once at TUI startup; `None` in tests.
    pub fn set_http_client(&mut self, client: entanglement_provider::HttpClient) {
        self.http_client = Some(client);
    }

    /// Injects the persisted default editor from `config.yml` (#persist-editor).
    /// Set once at TUI startup; blank/absent leaves env resolution in charge.
    pub fn set_configured_editor(&mut self, editor: Option<String>) {
        self.configured_editor = editor.filter(|s| !s.trim().is_empty());
    }

    /// The persisted default editor, if any — consulted by the `/editor`
    /// round-trip ahead of `$VISUAL`/`$EDITOR`.
    pub fn configured_editor(&self) -> Option<&str> {
        self.configured_editor.as_deref()
    }

    /// The most-throttled endpoint's live status, or `None` when every endpoint
    /// is at rest (or no client was injected). Polled each frame by
    /// `draw_input_info` so the indicator shows only while an endpoint backs off.
    pub fn throttle_status(&self) -> Option<entanglement_provider::ThrottleStatus> {
        self.http_client.as_ref()?.throttle_status()
    }

    /// Stores the chat viewport geometry + line provenance + rendered line text
    /// captured this frame so a later mouse click maps back to a transcript block
    /// and a drag-selection maps back to text.
    pub fn set_chat_hit_test(
        &mut self,
        area: Rect,
        scroll_offset: usize,
        line_blocks: Vec<Option<usize>>,
        line_text: Vec<String>,
    ) {
        self.chat_area = area;
        self.chat_scroll_offset = scroll_offset;
        self.chat_line_blocks = line_blocks;
        self.chat_line_text = line_text;
    }

    /// The current transcript selection, if any (for highlight rendering).
    pub fn selection(&self) -> Option<crate::tui::selection::Selection> {
        self.selection
    }

    /// Whether the active selection is a real drag (not a bare click) — the
    /// signal to copy on mouse-up rather than toggle a block.
    pub fn selection_moved(&self) -> bool {
        self.selection.is_some_and(|s| s.moved)
    }

    /// Map a terminal `(col, row)` to an absolute `(line_idx, char_col)` in the
    /// transcript, or `None` when the point is outside the chat area. Mirrors
    /// `block_at`'s bounds/offset math; columns are relative to the (already
    /// margin-adjusted) chat area.
    fn transcript_pos(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        let area = self.chat_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if col < area.x || col >= area.x + area.width {
            return None;
        }
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let line_idx = (row - area.y) as usize + self.chat_scroll_offset;
        let char_col = (col - area.x) as usize;
        Some((line_idx, char_col))
    }

    /// Begin a selection at `(col, row)` (mouse-down). A bare click leaves it
    /// zero-width (`moved == false`) so mouse-up can distinguish click vs drag.
    pub fn start_selection(&mut self, col: u16, row: u16) {
        self.selection = self
            .transcript_pos(col, row)
            .map(crate::tui::selection::Selection::new);
        self.mark_dirty();
    }

    /// Extend the active selection to `(col, row)` (mouse-drag), marking it moved.
    pub fn update_selection(&mut self, col: u16, row: u16) {
        if let (Some(pos), Some(sel)) = (self.transcript_pos(col, row), self.selection.as_mut()) {
            sel.cursor = pos;
            sel.moved = true;
            self.mark_dirty();
        }
    }

    /// Clear any active selection.
    pub fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.mark_dirty();
        }
    }

    /// The selected text (from this frame's rendered lines), recording a
    /// "Copied N chars" status line. `None`/empty selection ⇒ `None`, nothing
    /// recorded. The selection is left in place so its highlight persists until
    /// the next click.
    pub fn take_selection_text(&mut self) -> Option<String> {
        let sel = self.selection?;
        let text = crate::tui::selection::selection_text(&self.chat_line_text, &sel);
        if text.is_empty() {
            return None;
        }
        let chars = text.chars().count();
        self.record_status("copy", format!("Copied {chars} chars to clipboard"));
        Some(text)
    }

    /// Maps a terminal click at `(col, row)` to the collapsible block under it,
    /// or `None` when the click lands outside the chat area or on a non-block line.
    pub fn block_at(&self, col: u16, row: u16) -> Option<usize> {
        let area = self.chat_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if col < area.x || col >= area.x + area.width {
            return None;
        }
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let line_idx = (row - area.y) as usize + self.chat_scroll_offset;
        self.chat_line_blocks.get(line_idx).copied().flatten()
    }

    /// Keyboard fallback for the click: toggles the most recent collapsible
    /// block — a reasoning run or a tool op, whichever came last (#340).
    pub fn toggle_last_block(&mut self) {
        if let Some(id) = self.last_block_id() {
            self.toggle_block(id);
        }
    }

    /// Minting transcript index of the last collapsible block: the first delta
    /// of the last coalesced reasoning run, or the last `ToolCall` — whichever
    /// appears later in the transcript (#340).
    fn last_block_id(&self) -> Option<usize> {
        let mut last = None;
        let mut prev_was_reasoning = false;
        for (idx, entry) in self.transcript().iter().enumerate() {
            match entry {
                TranscriptEntry::ReasoningDelta { .. } => {
                    if !prev_was_reasoning {
                        last = Some(idx);
                    }
                    prev_was_reasoning = true;
                }
                TranscriptEntry::ToolCall { .. } => {
                    last = Some(idx);
                    prev_was_reasoning = false;
                }
                _ => prev_was_reasoning = false,
            }
        }
        last
    }
}
