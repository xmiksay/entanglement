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
