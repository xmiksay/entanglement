use crate::tui::session_view::TranscriptEntry;
use ratatui::layout::Rect;

use super::App;

impl App {
    pub fn scroll_offset(&self) -> usize {
        self.sessions.active_view().scroll_offset()
    }

    pub fn scroll_offset_x(&self) -> usize {
        self.sessions.active_view().scroll_offset_x()
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

    pub fn scroll_right(&mut self, cols: usize) {
        self.sessions.active_view_mut().scroll_right(cols);
        self.mark_dirty();
    }

    pub fn scroll_left(&mut self, cols: usize) {
        self.sessions.active_view_mut().scroll_left(cols);
        self.mark_dirty();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.sessions.active_view_mut().scroll_to_bottom();
        self.mark_dirty();
    }

    /// Whether the reasoning run `id` (transcript index of its first delta) is
    /// expanded in the active session.
    pub fn reasoning_expanded(&self, id: usize) -> bool {
        self.sessions.active_view().reasoning_expanded(id)
    }

    /// Flips a reasoning run between collapsed and expanded.
    pub fn toggle_reasoning_block(&mut self, id: usize) {
        self.sessions.active_view_mut().toggle_reasoning(id);
        self.mark_dirty();
    }

    /// Stores the chat viewport geometry + line provenance captured this frame
    /// so a later mouse click can be mapped back to a transcript block.
    pub fn set_chat_hit_test(
        &mut self,
        area: Rect,
        scroll_offset: usize,
        line_blocks: Vec<Option<usize>>,
    ) {
        self.chat_area = area;
        self.chat_scroll_offset = scroll_offset;
        self.chat_line_blocks = line_blocks;
    }

    /// Maps a terminal click at `(col, row)` to the reasoning block under it, or
    /// `None` when the click lands outside the chat area or on a non-block line.
    pub fn reasoning_block_at(&self, col: u16, row: u16) -> Option<usize> {
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

    /// Keyboard fallback for the click: toggles the most recent reasoning run.
    pub fn toggle_last_reasoning_block(&mut self) {
        if let Some(id) = self.last_reasoning_block_id() {
            self.toggle_reasoning_block(id);
        }
    }

    /// Transcript index of the first delta of the last coalesced reasoning run.
    fn last_reasoning_block_id(&self) -> Option<usize> {
        let mut last = None;
        let mut prev_was_reasoning = false;
        for (idx, entry) in self.transcript().iter().enumerate() {
            if matches!(entry, TranscriptEntry::ReasoningDelta { .. }) {
                if !prev_was_reasoning {
                    last = Some(idx);
                }
                prev_was_reasoning = true;
            } else {
                prev_was_reasoning = false;
            }
        }
        last
    }
}
