use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bash_live::BashRegistered;
use crate::tui::mention::{FileIndex, MentionPopup};

use super::App;

impl App {
    /// Wire the working directory into the head features that need it and
    /// record the shared bash-enablement handle `!bash` passthrough gates on
    /// (ADR-0030, #498). Called once by the event loop at startup. The `@file`
    /// completion index is *not* built here — the walk over a huge working
    /// directory used to freeze the first draw (#678); it arrives later via
    /// [`App::set_file_index`].
    pub fn init_head_context(&mut self, root: PathBuf, live_bash: Arc<BashRegistered>) {
        self.root = root;
        self.live_bash = live_bash;
    }

    /// Deliver the background-built `@file` index (#678), then re-derive the
    /// popup from the current input line so an already-typed `@query` opens
    /// the moment the index lands.
    pub fn set_file_index(&mut self, index: FileIndex) {
        self.mention.set_index(index);
        self.update_popups();
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `!bash` passthrough may run — the startup env var or a live
    /// `/enable tool bash` (#611/ADR-0163), either way (#498): reads the
    /// shared handle live, so a mid-session enable takes effect with no
    /// restart.
    pub fn bash_enabled(&self) -> bool {
        self.live_bash.get()
    }

    pub fn mention(&self) -> &MentionPopup {
        &self.mention
    }

    pub fn mention_mut(&mut self) -> &mut MentionPopup {
        &mut self.mention
    }

    pub fn mention_visible(&self) -> bool {
        self.mention.visible()
    }

    /// Recompute both the `@file` mention popup and the `/cmd` slash popup
    /// (Issue 2) from the current input line. Call after any key that changes
    /// the input text or cursor position — the one-call replacement for the
    /// scattered per-popup `update_*` sites.
    pub fn update_popups(&mut self) {
        let before = self.input.current_line_before_cursor().to_string();
        self.mention.update(&before);
        self.slash.update(&before);
        self.mark_dirty();
    }

    pub fn hide_mention(&mut self) {
        self.mention.hide();
        self.mark_dirty();
    }

    pub fn mention_select_next(&mut self) {
        self.mention.select_next();
        self.mark_dirty();
    }

    pub fn mention_select_prev(&mut self) {
        self.mention.select_prev();
        self.mark_dirty();
    }

    /// Swap the active `@query` token for the highlighted path (`@path `).
    /// Returns false (no-op) when the popup has no selection.
    pub fn accept_mention(&mut self) -> bool {
        let Some(path) = self.mention.selected().cloned() else {
            return false;
        };
        let before = self.input.current_line_before_cursor().to_string();
        if let Some(range) = crate::tui::mention::active_mention_range(&before) {
            self.input
                .replace_on_cursor_line(range.start, range.end, &format!("@{path} "));
        }
        self.mention.hide();
        self.mark_dirty();
        true
    }

    /// Record a `!bash` passthrough round-trip in the transcript (ADR-0030):
    /// the command and its captured output, rendered like a tool call/output.
    pub fn record_bash_passthrough(&mut self, command: String, output: String) {
        self.sessions
            .active_view_mut()
            .record_bash_passthrough(command, output);
        self.scroll_to_bottom();
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::App;
    use crate::tui::mention::FileIndex;
    use entanglement_core::SessionId;

    /// #678: the app is fully usable before the background-built index lands,
    /// and an already-typed `@query` opens the moment it does.
    #[test]
    fn input_works_before_file_index_ready() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.input().insert_str("see @a");
        app.update_popups();
        assert!(
            !app.mention_visible(),
            "popup must stay hidden while the index is empty"
        );
        app.set_file_index(FileIndex::from_paths(vec!["src/a.rs".into()]));
        assert!(
            app.mention_visible(),
            "popup should open once the index lands"
        );
        assert_eq!(app.mention().matches(), ["src/a.rs".to_string()]);
    }
}
