//! Transient input-info toast.
//!
//! A short-lived, non-transcript notice rendered on the bottom info line
//! (beside the throttle indicator). Used for feedback that must not become
//! transcript content — the drag-copy "Copied N chars" notice used to be a
//! `ToolOutput` entry, which is a hard segment boundary and split a streaming
//! Thinking block in two. Mirrors the two-stage-quit idiom (`quit.rs`): state
//! on `App`, lazy expiry in the getters, eager expiry in the render loop.

use std::time::{Duration, Instant};

use super::App;

/// How long a toast stays visible. Matches `quit::QUIT_TIMEOUT`'s feel.
pub const TOAST_TTL: Duration = Duration::from_secs(3);

impl App {
    /// Show `message` on the input info line for [`TOAST_TTL`].
    pub fn set_toast(&mut self, message: String) {
        self.toast = Some((message, Instant::now()));
        self.mark_dirty();
    }

    /// The toast to render, if one is set and unexpired.
    pub fn toast(&self) -> Option<&str> {
        match &self.toast {
            Some((msg, at)) if at.elapsed() < TOAST_TTL => Some(msg),
            _ => None,
        }
    }

    /// Whether a set toast has outlived its TTL. `false` when none is set, so
    /// the render loop's eager check doesn't mark dirty every frame.
    pub fn toast_expired(&self) -> bool {
        match &self.toast {
            Some((_, at)) => at.elapsed() >= TOAST_TTL,
            None => false,
        }
    }

    /// Drop the toast (expiry or explicit clear).
    pub fn clear_toast(&mut self) {
        if self.toast.take().is_some() {
            self.mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;

    #[test]
    fn set_toast_is_visible_and_marks_dirty() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.clear_dirty();
        app.set_toast("Copied 5 chars to clipboard".to_string());
        assert_eq!(app.toast(), Some("Copied 5 chars to clipboard"));
        assert!(app.is_dirty());
        assert!(!app.toast_expired());
    }

    #[test]
    fn expired_toast_neither_renders_nor_reports_fresh() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_toast("gone".to_string());
        // Age the timestamp past the TTL.
        app.toast = Some((
            "gone".to_string(),
            Instant::now() - TOAST_TTL - Duration::from_millis(1),
        ));
        assert_eq!(app.toast(), None, "expired toast never renders");
        assert!(app.toast_expired(), "eager loop check fires");
        app.clear_toast();
        assert!(!app.toast_expired(), "cleared toast is inert");
    }

    #[test]
    fn no_toast_is_not_expired() {
        // The render loop checks expiry every iteration; an absent toast must
        // not read as expired or it would mark dirty on every frame.
        let app = App::new_for_test(SessionId::new("s1"));
        assert!(!app.toast_expired());
        assert_eq!(app.toast(), None);
    }
}
