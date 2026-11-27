//! `App` surface for the slash-command completion popup (Issue 2). Mirrors
//! [`super::mention`]: visibility, update-from-input, navigation, and accept
//! (insert the selected command, replacing the `/…` token with `/<command> `).

use super::App;

impl App {
    pub fn slash(&self) -> &crate::tui::slash_popup::SlashPopup {
        &self.slash
    }

    pub fn slash_mut(&mut self) -> &mut crate::tui::slash_popup::SlashPopup {
        &mut self.slash
    }

    pub fn slash_visible(&self) -> bool {
        self.slash.visible()
    }

    /// Recompute the slash popup from the current input line. The Normal-mode
    /// event loop calls [`App::update_popups`] (which updates both popups); this
    /// is kept for focused test use.
    #[cfg(test)]
    pub fn update_slash(&mut self) {
        let before = self.input.current_line_before_cursor().to_string();
        self.slash.update(&before);
        self.mark_dirty();
    }

    pub fn hide_slash(&mut self) {
        self.slash.hide();
        self.mark_dirty();
    }

    pub fn slash_select_next(&mut self) {
        self.slash.select_next();
        self.mark_dirty();
    }

    pub fn slash_select_prev(&mut self) {
        self.slash.select_prev();
        self.mark_dirty();
    }

    /// Swap the active `/prefix` token for the selected command
    /// (`/<command> `). Returns false (no-op) when the popup has no selection.
    pub fn accept_slash(&mut self) -> bool {
        let Some(cmd) = self.slash.selected().cloned() else {
            return false;
        };
        let before = self.input.current_line_before_cursor().to_string();
        if let Some(range) = crate::tui::slash_popup::active_slash_range(&before) {
            self.input
                .replace_on_cursor_line(range.start, range.end, &format!("/{} ", cmd.name()));
        }
        self.slash.hide();
        self.mark_dirty();
        true
    }
}
