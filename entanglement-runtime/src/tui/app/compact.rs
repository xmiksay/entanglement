//! Following a compaction into its successor session (ADR-0205).
//!
//! The head no longer forks anything. Every compaction — manual `/compact`,
//! auto-summarize on overflow, the prune-only fallback — is the *engine*
//! minting a successor session and retiring the source, so all the TUI does is
//! follow along: remember the summary when `Compacted` arrives, then, when the
//! successor announces itself with `predecessor` pointing at that source, open
//! its view and seed the transcript with what it is continuing from.
//!
//! Following lineage rather than a fork this head issued is what makes it work
//! for a compaction the user never asked for and may not even be watching — a
//! background session that overflowed mid-turn gets its view either way, and
//! only a compaction of the *active* session moves the user.

use entanglement_core::SessionId;

use super::App;

impl App {
    /// Records a `/compact` parse error (bad `--keep` value) as a transcript
    /// status line (#397) — no engine traffic, mirrors
    /// `App::record_set_error`'s wrapper pattern.
    pub fn record_compact_error(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("compact", format!("error: {message}"));
        self.mark_dirty();
    }

    /// Remember a compaction's seed text against the session that compacted,
    /// for [`follow_successor`][Self::follow_successor] to hand to the
    /// successor once it appears. The notice on the source's own view is
    /// rendered separately by the reducer's `Compacted` arm.
    pub(crate) fn note_compaction_seed(&mut self, source: SessionId, seed: String) {
        self.compaction_seeds.insert(source, seed);
        self.mark_dirty();
    }

    /// Open the successor of a compaction and, if the user was watching the
    /// session that compacted, move them into it.
    ///
    /// The successor's first user message is seeded head-side because the
    /// engine never echoes an `InMsg` back as an `OutEvent`: the seed rides
    /// the successor's `Spawn` prompt, so without this the transcript would
    /// open on the model's reply to a prompt the user never saw. Same pattern
    /// as the `propose_plan` handoff and an ordinary user prompt.
    pub(crate) fn follow_successor(&mut self, source: SessionId, successor: SessionId) {
        self.sessions.ensure(&successor);
        if let Some(seed) = self.compaction_seeds.remove(&source) {
            if let Some(view) = self.sessions.view_for_mut(&successor) {
                view.record_user_message(seed);
            }
        }
        // A background session compacting must not yank the view out from
        // under whatever the user is reading.
        if self.sessions.active_id() == &source {
            self.sessions.switch_to(successor);
        }
        self.mark_dirty();
    }

    /// Seeds recorded but not yet claimed by a successor. Test-only window
    /// onto the hand-off's one piece of state.
    #[cfg(test)]
    pub(crate) fn compaction_seeds(&self) -> &std::collections::HashMap<SessionId, String> {
        &self.compaction_seeds
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::app::App;
    use crate::tui::session_view::TranscriptEntry;
    use entanglement_core::{AgentState, CompactionMode, OutEvent, SessionId};

    fn compacted(session: &str, seq: u64, summary: &str, auto: bool) -> OutEvent {
        OutEvent::Compacted {
            session: SessionId::new(session),
            seq,
            summary: summary.into(),
            kept: 0,
            auto,
            mode: CompactionMode::Summary,
        }
    }

    fn started(session: &str, predecessor: Option<&str>) -> OutEvent {
        OutEvent::SessionStarted {
            session: SessionId::new(session),
            parent: None,
            predecessor: predecessor.map(SessionId::new),
            profile: "build".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
            sponsored: false,
        }
    }

    fn first_user_text(app: &App, session: &str) -> Option<String> {
        app.sessions
            .view_for(&SessionId::new(session))?
            .transcript()
            .iter()
            .find_map(|e| match e {
                TranscriptEntry::User { text, .. } => Some(text.clone()),
                _ => None,
            })
    }

    /// The whole hand-off: the source renders its notice, the successor gets a
    /// view seeded with the summary, and the user moves into it.
    #[test]
    fn the_successor_of_the_active_session_is_seeded_and_becomes_active() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(OutEvent::Status {
            session: SessionId::new("s1"),
            state: AgentState::Done,
        });

        app.handle_out_event(compacted("s1", 1, "user asked for X, agent did Y", false));
        // Nothing moves until the engine's successor actually appears.
        assert_eq!(app.active_session_id(), &SessionId::new("s1"));

        app.handle_out_event(started("s2", Some("s1")));

        assert_eq!(
            app.active_session_id(),
            &SessionId::new("s2"),
            "the user follows the compaction forward"
        );
        let seeded = first_user_text(&app, "s2").expect("the successor is seeded");
        assert!(seeded.contains("user asked for X, agent did Y"), "{seeded}");
        assert!(
            app.compaction_seeds().is_empty(),
            "the seed is consumed by the successor"
        );

        // The source's view survives, carrying the compaction notice.
        let source = app
            .sessions
            .view_for(&SessionId::new("s1"))
            .expect("source view survives");
        assert!(source.transcript().iter().any(|e| matches!(
            e,
            TranscriptEntry::ToolOutput { tool: Some(t), .. } if t == "compact"
        )));
    }

    /// The automatic paths take the same route — there is no `auto: true`
    /// special case any more, because the engine forks on every path.
    #[test]
    fn an_auto_compaction_follows_its_successor_too() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(compacted("s1", 1, "overflowed, summarized", true));
        app.handle_out_event(started("s2", Some("s1")));

        assert_eq!(app.active_session_id(), &SessionId::new("s2"));
        let seeded = first_user_text(&app, "s2").expect("the successor is seeded");
        assert!(seeded.contains("overflowed, summarized"));
    }

    /// A background session compacting gets its successor view, but must not
    /// steal the user's place.
    #[test]
    fn a_background_compaction_does_not_switch_the_active_view() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(compacted("bg", 1, "background summary", true));
        app.handle_out_event(started("bg2", Some("bg")));

        assert_eq!(
            app.active_session_id(),
            &SessionId::new("s1"),
            "the active view stays put"
        );
        let seeded = first_user_text(&app, "bg2").expect("the successor still gets its view");
        assert!(seeded.contains("background summary"));
    }

    /// A replayed/lagged duplicate must not overwrite a newer seed.
    #[test]
    fn a_replayed_compacted_does_not_clobber_the_recorded_seed() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(compacted("s1", 5, "the real summary", false));
        app.handle_out_event(compacted("s1", 1, "a stale replay", false));
        app.handle_out_event(started("s2", Some("s1")));

        let seeded = first_user_text(&app, "s2").expect("the successor is seeded");
        assert!(seeded.contains("the real summary"), "{seeded}");
    }

    /// An ordinary session start (no lineage) is not a compaction hand-off.
    #[test]
    fn a_plain_session_start_changes_nothing() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(started("other", None));
        assert_eq!(app.active_session_id(), &SessionId::new("s1"));
    }
}
