//! Compaction fork — copy-on-write (ADR-0101).
//!
//! On `OutEvent::Compacted`, the TUI forks the summary into a fresh session:
//! the source session's `Context` was never mutated (the engine's `compact_op`
//! emits the summary as a report, not a mutation), so the fork is the *only*
//! place the summary lands as a prompt. The source view stays intact and the
//! new view is switched to — mirroring the `propose_plan` handoff (#141),
//! except here the fork is a **child** of the source (lineage: `Spawn`'s
//! `parent` records the source id), not a fresh root.

use entanglement_core::{InMsg, SessionId};

use super::{App, CompactFork};

impl App {
    /// Fork a `Compacted` summary into a new session (ADR-0101): mint a fresh
    /// id, record the summary as its first user message, switch the active view
    /// to the new session, and record a pending `Spawn` for the async main loop
    /// to send. The engine `Spawn` inherits the source's profile so the fork
    /// runs under the same model pin, and seeds the summary as the first prompt.
    ///
    /// The fork notice on the source view is rendered by the reducer's
    /// `Compacted` arm (this runs before the event reaches the reducer; the
    /// reducer renders on the same `Compacted` once `sessions.handle_out_event`
    /// routes it).
    pub(crate) fn handle_compacted(&mut self, source: SessionId, summary: String) {
        // The source session's current agent profile name — `Spawn` inherits it
        // so the fork runs under the same profile/model pin as the source.
        let agent = self
            .sessions
            .view_for(&source)
            .map(|v| v.agent().to_string())
            .unwrap_or_else(|| "build".to_string());

        let new_session = SessionId::new_uuid();

        // Adopt the new session head-side: create its view and switch to it.
        self.sessions.adopt(new_session.clone());
        // Record the summary as the new session's first user message so it
        // shows in the scrollback (the engine's `Spawn` seeds it as the first
        // prompt; the engine never echoes `InMsg` back as an `OutEvent`, so the
        // head mirrors it locally — same pattern as `propose_plan` handoff and
        // an ordinary user prompt).
        let summary_msg = wrap_compaction_summary(&summary);
        if let Some(new_view) = self.sessions.view_for_mut(&new_session) {
            new_view.record_user_message(summary_msg.clone());
        }

        self.pending_compact_fork = Some(CompactFork {
            new_session: new_session.clone(),
            source,
            agent,
            summary: summary_msg,
        });
        self.mark_dirty();
    }

    /// Take the pending compaction fork, if any — the async main loop drains
    /// this and sends the `InMsg::Spawn` that actually creates the forked
    /// session in the engine. Returns `None` once drained.
    pub fn take_pending_compact_fork(&mut self) -> Option<CompactFork> {
        self.pending_compact_fork.take()
    }

    /// Build the `InMsg::Spawn` for a recorded compaction fork (ADR-0101):
    /// the summary seeds the forked session's first user message, the fork is a
    /// child of the source (`parent` = source id) for lineage, and the source's
    /// agent profile is inherited.
    pub fn spawn_for_fork(fork: &CompactFork) -> InMsg {
        InMsg::Spawn {
            session: fork.new_session.clone(),
            parent: fork.source.clone(),
            agent: fork.agent.clone(),
            prompt: fork.summary.clone(),
        }
    }
}

/// Wrap a raw compaction summary into the forked session's first user message.
/// Mirrors the framing the old in-place `apply_compaction` used, so the forked
/// session starts from a self-describing prompt.
pub(crate) fn wrap_compaction_summary(summary: &str) -> String {
    format!(
        "[Conversation summary — this session continues from a compaction of an \
         earlier session]\n\n{summary}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::session_view::TranscriptEntry;
    use entanglement_core::{AgentState, OutEvent, SessionId};

    #[test]
    fn compacted_event_forks_into_a_new_session_and_preserves_the_source() {
        let mut app = App::new_for_test(SessionId::new("s1"));

        app.handle_out_event(OutEvent::Status {
            session: SessionId::new("s1"),
            state: AgentState::Done,
        });

        app.handle_out_event(OutEvent::Compacted {
            session: SessionId::new("s1"),
            seq: 1,
            summary: "user asked for X, agent did Y".into(),
            kept: 0,
        });

        // The view switched to the fresh fork session.
        let active = app.active_session_id().clone();
        assert_ne!(
            active,
            SessionId::new("s1"),
            "the fork became the active session"
        );

        // The source session's view still exists and carries the fork notice
        // (rendered by the reducer's `Compacted` arm).
        let src_view = app
            .sessions
            .view_for(&SessionId::new("s1"))
            .expect("source session view survives");
        let notice = src_view.transcript().iter().find_map(|e| match e {
            TranscriptEntry::ToolOutput {
                tool: Some(t),
                output,
            } if t == "compact" => Some(output.clone()),
            _ => None,
        });
        assert!(
            notice
                .as_ref()
                .map(|n| n.contains("forked"))
                .unwrap_or(false),
            "source view renders a fork notice: {notice:?}"
        );

        // The forked session's view carries the summary as its first user message.
        let new_view = app
            .sessions
            .view_for(&active)
            .expect("forked session view exists");
        let first = new_view
            .transcript()
            .first()
            .expect("the forked session has the summary seeded");
        match first {
            TranscriptEntry::User { text, .. } => {
                assert!(text.contains("user asked for X, agent did Y"));
            }
            other => panic!("first entry should be the seeded summary: {other:?}"),
        }

        // A pending fork was recorded for the main loop to send.
        let fork = app
            .take_pending_compact_fork()
            .expect("a pending fork was recorded");
        assert_eq!(fork.source, SessionId::new("s1"));
        assert_eq!(fork.new_session, active);
        assert_eq!(fork.agent, "build");
        assert!(fork.summary.contains("user asked for X, agent did Y"));
        // The spawn is addressed to the fork under the source's profile, as a
        // child of the source.
        let spawn = App::spawn_for_fork(&fork);
        match spawn {
            InMsg::Spawn {
                session,
                parent,
                agent,
                prompt,
            } => {
                assert_eq!(session, active);
                assert_eq!(parent, SessionId::new("s1"));
                assert_eq!(agent, "build");
                assert!(prompt.contains("user asked for X, agent did Y"));
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        assert!(
            app.take_pending_compact_fork().is_none(),
            "the fork is drained"
        );
    }

    #[test]
    fn compacted_event_is_deduped_on_replay() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(OutEvent::Compacted {
            session: SessionId::new("s1"),
            seq: 1,
            summary: "first".into(),
            kept: 0,
        });
        let first_fork = app.active_session_id().clone();
        // The same event replayed (seq not advancing) must not fork again.
        app.handle_out_event(OutEvent::Compacted {
            session: SessionId::new("s1"),
            seq: 1,
            summary: "replay".into(),
            kept: 0,
        });
        assert_eq!(
            app.active_session_id(),
            &first_fork,
            "a replayed Compacted does not fork a second time"
        );
    }
}
