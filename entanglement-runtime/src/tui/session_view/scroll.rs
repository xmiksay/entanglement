use super::SessionView;

impl SessionView {
    /// Largest valid top-anchored offset for the last-drawn content: the line
    /// index at which the final line sits on the bottom row of the viewport.
    fn max_offset(&self) -> usize {
        self.last_content_height
            .saturating_sub(self.last_viewport_height)
    }

    /// The offset the view should actually render at, resolving auto-follow
    /// (pinned to the bottom) and clamping a frozen offset to the content.
    pub fn effective_scroll_offset(&self) -> usize {
        if self.auto_follow {
            self.max_offset()
        } else {
            self.scroll_offset.min(self.max_offset())
        }
    }

    /// Caches the metrics `draw_body` measured this frame. A resize/shrink that
    /// leaves a frozen view sitting at the bottom re-arms follow.
    pub fn set_viewport_metrics(&mut self, content_height: usize, viewport_height: usize) {
        self.last_content_height = content_height;
        self.last_viewport_height = viewport_height;
        if !self.auto_follow && self.scroll_offset >= self.max_offset() {
            self.auto_follow = true;
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let max = self.max_offset();
        let next = (self.effective_scroll_offset() + lines).min(max);
        self.scroll_offset = next;
        // Reaching the last line re-arms follow; otherwise stay frozen.
        self.auto_follow = next >= max;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        // Anchor at the currently displayed position before moving: while
        // auto-following the stored offset is stale (draw uses `max_offset`).
        self.scroll_offset = self.effective_scroll_offset().saturating_sub(lines);
        self.auto_follow = false;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.max_offset();
        self.auto_follow = true;
    }
}
