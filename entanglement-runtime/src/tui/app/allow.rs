//! `App` surface for the `/allow` command (#486, ADR-0126): unlike `/mcp`'s
//! wire ops, a directory grant is recorded synchronously through the
//! installed [`DefaultGrantStore`] handle — no engine round-trip — so this
//! module is a setter plus the status-line render, mirroring `app/mcp.rs`'s
//! shape without any `OutEvent` folding.

use std::sync::Arc;

use crate::policy::{DefaultGrantStore, GrantStore};

use super::App;

impl App {
    /// Install the shared grant store (#486), threaded in from the head so
    /// `/allow` can record a `SessionDir` grant directly — the same handle
    /// the tool executor's `Ask` upgrade reads.
    pub fn set_grants(&mut self, grants: Arc<DefaultGrantStore>) {
        self.grants = Some(grants);
    }

    /// Record a `SessionDir` grant for `dir` (already normalized root-relative,
    /// #485) against the active session, and render the confirmation as a
    /// transcript status line — the note flags a not-yet-created directory
    /// rather than rejecting it (ADR-0126 grants directories that don't exist
    /// yet). A missing grant store (never true outside tests) renders as an
    /// error instead of silently doing nothing.
    pub(crate) fn apply_allow_grant(&mut self, dir: &str) {
        let Some(grants) = self.grants.clone() else {
            self.record_allow_error("no grant store installed".to_string());
            return;
        };
        let session = self.active_session_id().clone();
        let stored = grants.grant_session_dir(&session, dir);
        let note = if self.root().join(&stored).exists() {
            ""
        } else {
            " (path does not exist yet)"
        };
        self.record_status(
            "allow",
            format!("granted read/grep/glob under '{stored}' for this session{note}"),
        );
        self.mark_dirty();
    }

    /// Records an `/allow` parse or outside-root error (#486) as a transcript
    /// status line — no grant store touched, mirroring `App::record_mcp_error`.
    pub(crate) fn record_allow_error(&mut self, message: String) {
        self.record_status("allow", format!("error: {message}"));
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use entanglement_core::SessionId;

    use super::*;

    #[test]
    fn apply_allow_grant_records_a_transcript_status_line() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_grants(Arc::new(DefaultGrantStore::load()));
        app.apply_allow_grant("src");
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("granted read/grep/glob under 'src'"));
        assert!(rendered, "expected a transcript entry noting the grant");
    }

    #[test]
    fn apply_allow_grant_without_a_store_records_an_error() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.apply_allow_grant("src");
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("no grant store installed"));
        assert!(
            rendered,
            "expected the missing-store error in the transcript"
        );
    }

    #[test]
    fn record_allow_error_records_a_transcript_status_line() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.record_allow_error("outside the project root: /etc".to_string());
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("outside the project root"));
        assert!(rendered, "expected the parse error in the transcript");
    }
}
