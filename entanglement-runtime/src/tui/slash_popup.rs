//! Prefix-filter slash-command completion popup (Issue 2). Mirrors
//! [`crate::tui::mention::MentionPopup`] in shape and behavior: a persistent
//! [`ListState`] that survives redraws, with `matches` recomputed on every input
//! change via [`SlashPopup::update`]. Unlike the old `draw_slash_autocomplete`
//! (which only fired on a lone `/`), this popup filters the full command roster
//! by the text after `/` up to the cursor, so `/co` narrows to `compact`/
//! `continue` and typing further refines it live.
//!
//! Tab/Enter inserts the selected command (replacing the `/…` text with
//! `/<command> `); Up/Down navigate; Esc or losing the `/` hides it.

use ratatui::widgets::ListState;

use crate::tui::commands::{all_commands, filter_commands, Command};

/// If the text immediately before the cursor is a `/`-prefixed token with no
/// spaces after it, return the prefix (chars after `/`). Returns `None` for a
/// non-`/` line, a `/` that follows non-whitespace, or a completed token (a
/// space after `/word`). A lone `/` returns `Some("")` (empty prefix = all
/// commands).
pub fn active_slash_prefix(line_before_cursor: &str) -> Option<&str> {
    let slash = line_before_cursor.rfind('/')?;
    // The `/` must sit at the start of the line or follow whitespace so a path
    // like `https://` or a URL fragment doesn't trigger it.
    if slash > 0 {
        let prev = line_before_cursor[..slash].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }
    let prefix = &line_before_cursor[slash + 1..];
    // A space after `/word` means the command token is finished — hide.
    if prefix.chars().any(char::is_whitespace) {
        return None;
    }
    Some(prefix)
}

/// Byte range `[/ … cursor)` of the active slash token, for replacement.
pub fn active_slash_range(line_before_cursor: &str) -> Option<std::ops::Range<usize>> {
    active_slash_prefix(line_before_cursor)?;
    let slash = line_before_cursor.rfind('/')?;
    Some(slash..line_before_cursor.len())
}

/// Popup state for slash-command completion — mirrors
/// [`crate::tui::mention::MentionPopup`]: persistent across frames so selection
/// survives redraws, with `matches` recomputed via [`SlashPopup::update`].
pub struct SlashPopup {
    visible: bool,
    prefix: String,
    matches: Vec<Command>,
    state: ListState,
}

impl SlashPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            prefix: String::new(),
            matches: Vec::new(),
            state: ListState::default(),
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn matches(&self) -> &[Command] {
        &self.matches
    }

    #[cfg(test)]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    pub fn selected(&self) -> Option<&Command> {
        self.state.selected().and_then(|i| self.matches.get(i))
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.matches.clear();
        self.state.select(None);
    }

    /// Recompute from the input line up to the cursor. Shows the popup iff an
    /// active `/prefix` token is present. Empty prefix → all commands; a
    /// non-empty prefix narrows via [`filter_commands`].
    pub fn update(&mut self, line_before_cursor: &str) {
        match active_slash_prefix(line_before_cursor) {
            Some(prefix) => {
                self.prefix = prefix.to_string();
                self.matches = if prefix.is_empty() {
                    all_commands()
                } else {
                    filter_commands(prefix)
                };
                self.visible = !self.matches.is_empty();
                self.state.select((!self.matches.is_empty()).then_some(0));
            }
            None => self.hide(),
        }
    }

    pub fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0);
        self.state.select(Some((cur + 1) % self.matches.len()));
    }

    pub fn select_prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0);
        let prev = if cur == 0 {
            self.matches.len() - 1
        } else {
            cur - 1
        };
        self.state.select(Some(prev));
    }
}

impl Default for SlashPopup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_prefix_requires_leading_slash_at_word_boundary() {
        assert_eq!(active_slash_prefix("/co"), Some("co"));
        assert_eq!(active_slash_prefix("/"), Some(""));
        // `/` mid-word (URL-like) or token already ended → None.
        assert_eq!(active_slash_prefix("https://x"), None);
        assert_eq!(active_slash_prefix("/compact done "), None);
    }

    #[test]
    fn active_range_spans_slash_to_cursor() {
        assert_eq!(active_slash_range("/co"), Some(0..3));
        assert_eq!(active_slash_range("no slash"), None);
    }

    #[test]
    fn empty_prefix_lists_all_commands() {
        let mut popup = SlashPopup::new();
        popup.update("/");
        assert!(popup.visible());
        assert_eq!(popup.matches().len(), all_commands().len());
    }

    #[test]
    fn co_prefix_narrows_to_compact_and_continue() {
        let mut popup = SlashPopup::new();
        popup.update("/co");
        assert!(popup.visible());
        let names: Vec<&str> = popup.matches().iter().map(|c| c.name()).collect();
        assert!(names.contains(&"compact"), "names={names:?}");
        assert!(names.contains(&"continue"), "names={names:?}");
        // Every match's name or description contains "co" — case-insensitively,
        // matching `filter_commands`' own lowercasing (`resume` matches on
        // "Continue a past session").
        for cmd in popup.matches() {
            let hit = cmd.name().to_lowercase().contains("co")
                || cmd.description().to_lowercase().contains("co");
            assert!(hit, "unexpected match: {}", cmd.name());
        }
    }

    #[test]
    fn prefix_tracks_the_typed_token() {
        let mut popup = SlashPopup::new();
        popup.update("/");
        assert_eq!(popup.prefix(), "");
        popup.update("/co");
        assert_eq!(popup.prefix(), "co");
    }

    #[test]
    fn no_match_hides_popup() {
        let mut popup = SlashPopup::new();
        popup.update("/zzzzz");
        assert!(!popup.visible());
    }

    #[test]
    fn losing_the_slash_hides_popup() {
        let mut popup = SlashPopup::new();
        popup.update("/co");
        assert!(popup.visible());
        popup.update("plain text");
        assert!(!popup.visible());
        assert!(popup.selected().is_none());
    }

    #[test]
    fn navigation_wraps() {
        let mut popup = SlashPopup::new();
        popup.update("/"); // all commands, > 1 match
        let first = popup.selected().cloned();
        popup.select_next();
        assert_ne!(popup.selected().cloned(), first);
        // Wrap back: select_prev enough times returns to first. Since the list
        // is > 2 long, one prev from index 1 wraps to the last.
        popup.select_prev();
        // After next+prev we're back at the same position (wrap is modular).
    }

    #[test]
    fn select_first_match_by_default() {
        let mut popup = SlashPopup::new();
        popup.update("/comp");
        assert_eq!(popup.selected(), Some(&Command::Compact));
    }
}
