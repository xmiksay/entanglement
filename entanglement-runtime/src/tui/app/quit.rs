//! Two-stage Ctrl+C handling (ADR-0087).
//!
//! A first Ctrl+C does not quit: it clears the transient input state (text
//! buffer, `@file` popup, multiline mode) and arms a pending quit. A second
//! Ctrl+C within [`QUIT_TIMEOUT`] quits from any context. Any other key — or
//! expiry of the timeout — disarms, so the next Ctrl+C is treated as a fresh
//! first press. Ctrl+Q remains an unconditional immediate quit (the escape
//! hatch), and an external `SIGINT` (`kill -INT`) routes through
//! [`App::handle_quit_key`] too, so it never leaves the terminal in raw mode.
//!
//! The intercept lives once at the top of `handle_event`'s key-press block, so
//! behaviour is identical across normal mode, every modal/picker, and every
//! approval prompt — replacing the eleven duplicate `Char('c') | Char('q')`
//! arms that used to quit on the first press.

use std::time::{Duration, Instant};

use crate::tui::input::SimpleInput;

use super::App;

/// Window in which a second Ctrl+C counts as "press again to quit". Lazily
/// checked in [`App::handle_quit_key`] (correctness) and eagerly in the render
/// loop (so the hint disappears promptly after expiry).
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(3);

impl App {
    /// Process a Ctrl+C / external `SIGINT`.
    ///
    /// Returns `true` when the app should quit (a second press within the
    /// window); `false` when it only cleared input + armed (a first press, or
    /// a press after the prior arming expired). Does **not** close modals —
    /// `Esc` already does that everywhere, and keeping the modal visible on the
    /// first press matches "clear, don't discard".
    pub fn handle_quit_key(&mut self) -> bool {
        if self.quit_pending && !self.quit_pending_expired() {
            return true;
        }
        // First press (or the prior arming expired): clear transient input and
        // re-arm.
        self.input = SimpleInput::default();
        self.input_multiline = false;
        self.mention.hide();
        self.quit_pending = true;
        self.quit_pending_at = Some(Instant::now());
        self.mark_dirty();
        false
    }

    /// Disarm a pending quit. Called by the event loop for any non-Ctrl+C key
    /// so a stale arming can't turn a later Ctrl+C into an accidental quit
    /// (e.g. type text → Ctrl+C → type more → Ctrl+C must be a first press).
    pub fn clear_quit_pending(&mut self) {
        if self.quit_pending {
            self.quit_pending = false;
            self.quit_pending_at = None;
            self.mark_dirty();
        }
    }

    /// Whether the pending-quit window has elapsed. `false` when no quit is
    /// pending.
    pub fn quit_pending_expired(&self) -> bool {
        match self.quit_pending_at {
            Some(at) => at.elapsed() >= QUIT_TIMEOUT,
            None => true,
        }
    }

    /// Whether a "press Ctrl+C again to quit" hint should render.
    pub fn quit_pending(&self) -> bool {
        self.quit_pending && !self.quit_pending_expired()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::mention::{FileIndex, MentionPopup};
    use entanglement_core::SessionId;

    #[test]
    fn first_press_clears_input_and_arms() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.input.insert_str("half-typed prompt");
        app.set_input_multiline(true);
        app.mention = MentionPopup::new(FileIndex::from_paths(vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
        ]));
        app.input.insert_str(" @a");
        app.update_mention();
        assert!(app.mention_visible(), "precondition: popup open");

        // First press does not quit; it clears and arms.
        assert!(!app.handle_quit_key());
        assert_eq!(app.input_text(), "", "text buffer cleared");
        assert!(!app.is_input_multiline(), "multiline mode cleared");
        assert!(!app.mention_visible(), "mention popup hidden");
        assert!(app.quit_pending(), "quit is now armed");
    }

    #[test]
    fn second_press_within_window_quits() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        assert!(!app.handle_quit_key(), "first press arms");
        assert!(app.handle_quit_key(), "second press quits");
    }

    #[test]
    fn any_other_key_disarms() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        assert!(!app.handle_quit_key());
        assert!(app.quit_pending());

        app.clear_quit_pending();
        assert!(!app.quit_pending());

        // After disarming, a Ctrl+C is a fresh first press (arms, not quits).
        assert!(!app.handle_quit_key());
    }

    #[test]
    fn empty_input_still_arms() {
        // Even with nothing to clear, the first press arms so a second can quit.
        let mut app = App::new_for_test(SessionId::new("s1"));
        assert_eq!(app.input_text(), "");
        assert!(!app.handle_quit_key());
        assert!(app.quit_pending());
        assert!(app.handle_quit_key(), "second press quits from empty input");
    }

    #[test]
    fn clear_is_a_noop_when_not_armed() {
        // Clearing with no quit pending must not mark dirty (no spurious redraw).
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.clear_dirty();
        app.clear_quit_pending();
        assert!(!app.is_dirty(), "no redraw from a no-op clear");
    }

    #[test]
    fn expired_arming_treated_as_first_press() {
        // Simulate an arming that aged past the window without the render loop
        // clearing it: the next press must arm afresh, not quit.
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_quit_key();
        // Force the timestamp into the past beyond the timeout.
        app.quit_pending_at = Some(Instant::now() - QUIT_TIMEOUT - Duration::from_millis(1));

        assert!(app.quit_pending_expired(), "precondition: expired");
        assert!(
            !app.handle_quit_key(),
            "an expired arming re-arms instead of quitting"
        );
        assert!(app.quit_pending(), "re-armed with a fresh window");
    }
}
