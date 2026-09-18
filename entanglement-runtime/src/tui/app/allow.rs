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
    /// #485) against the active session's *current mode* (#634, ADR-0207 §8:
    /// a directory grant is mode-scoped like every other grant), and render
    /// the confirmation as a transcript status line — the note flags a
    /// not-yet-created directory rather than rejecting it (ADR-0126 grants
    /// directories that don't exist yet). A missing grant store (never true
    /// outside tests) renders as an error instead of silently doing nothing.
    pub(crate) fn apply_allow_grant(&mut self, dir: &str) {
        let Some(grants) = self.grants.clone() else {
            self.record_allow_error("no grant store installed".to_string());
            return;
        };
        let session = self.active_session_id().clone();
        let mode = self.mode().to_string();
        let stored = grants.grant_session_dir(&session, dir, &mode);
        let note = if self.root().join(&stored).exists() {
            ""
        } else {
            " (path does not exist yet)"
        };
        self.set_toast(format!(
            "granted read/grep/glob under '{stored}' for this session{note}"
        ));
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
    use entanglement_core::{OutEvent, SessionId};

    use super::*;

    /// #634: a `SessionDir` grant recorded via `/allow` is scoped to the
    /// active session's *current* mode, matching `GrantKey`'s own scoping —
    /// it fires under the mode it was earned in and not another, and a later
    /// `/allow` after a mode switch is scoped to the new one.
    #[test]
    fn apply_allow_grant_scopes_to_the_active_mode() {
        let session = SessionId::new("s1");
        let mut app = App::new_for_test(session.clone());
        let grants = Arc::new(DefaultGrantStore::load());
        app.set_grants(grants.clone());

        // No `ModeChanged` folded yet — the view's default mirrors the
        // engine's own `DEFAULT_MODE` ("build").
        app.apply_allow_grant("src");
        assert!(grants.is_granted(&session, "read", Some("src/a.rs"), "build"));
        assert!(
            !grants.is_granted(&session, "read", Some("src/a.rs"), "research"),
            "a directory grant earned in build must not fire in research"
        );

        // Switching mode changes what a *later* `/allow` is scoped to.
        app.handle_out_event(OutEvent::ModeChanged {
            session: session.clone(),
            mode: "research".to_string(),
        });
        app.apply_allow_grant("docs");
        assert!(grants.is_granted(&session, "read", Some("docs/a.rs"), "research"));
        assert!(!grants.is_granted(&session, "read", Some("docs/a.rs"), "build"));
    }

    #[test]
    fn apply_allow_grant_toasts_the_grant() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.set_grants(Arc::new(DefaultGrantStore::load()));
        app.apply_allow_grant("src");
        assert!(
            app.toast()
                .is_some_and(|t| t.contains("granted read/grep/glob under 'src'")),
            "expected a toast noting the grant"
        );
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
