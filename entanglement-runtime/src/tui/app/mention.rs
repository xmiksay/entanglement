use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bash_live::LiveBashState;
use crate::tui::mention::{FileIndex, MentionPopup};

use super::App;

impl App {
    /// Wire the working directory into the head features that need it: builds
    /// the `@file` completion index and records the shared bash-enablement
    /// handle `!bash` passthrough gates on (ADR-0030, #498). Called once by
    /// the event loop at startup.
    pub fn init_head_context(&mut self, root: PathBuf, live_bash: Arc<LiveBashState>) {
        self.mention = MentionPopup::new(FileIndex::build(&root));
        self.root = root;
        self.live_bash = live_bash;
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `!bash` passthrough may run — the startup env var or a live
    /// `/bash on`, either way (#498): reads the shared handle live, so a
    /// mid-session toggle takes effect with no restart.
    pub fn bash_enabled(&self) -> bool {
        self.live_bash.is_enabled()
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

    /// Recompute the `@file` popup from the current input line. Call after any
    /// key that changes the input text or cursor position.
    pub fn update_mention(&mut self) {
        let before = self.input.current_line_before_cursor().to_string();
        self.mention.update(&before);
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
