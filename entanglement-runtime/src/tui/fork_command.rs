//! `/fork [N]` (#786, ADR-0223): copy the active session into a new root via
//! `InMsg::ForkSession` — the whole history, or turns `1..=N` only. The engine
//! takes the fork once the session is idle and announces it with
//! `SessionForked`, which moves the user into the fork (`app/out_event.rs`)
//! and renders the inherited history (`session_view::reducer::fork`).

use entanglement_core::{AgentState, Holly, InMsg};

use crate::tui::app::App;

/// Send the fork, or record a usage error for a bad turn argument.
pub(crate) async fn send_fork(app: &mut App, holly: &Holly, text: &str) {
    let at_turn = match parse_fork_args(text) {
        Ok(at_turn) => at_turn,
        Err(e) => {
            app.record_status("fork", e);
            return;
        }
    };
    let session = app.active_session_id().clone();
    let state = app
        .sessions()
        .into_iter()
        .find(|(id, _)| **id == session)
        .map(|(_, view)| view.state());
    // The engine defers a fork until the session rests, so say why nothing
    // happens yet instead of leaving the command looking ignored.
    if let Some(note) = state.and_then(deferral_note) {
        app.record_status("fork", note.to_string());
    }
    let _ = holly.send(InMsg::ForkSession { session, at_turn }).await;
}

/// Why a fork sent now waits, or `None` when the session is at rest.
fn deferral_note(state: AgentState) -> Option<&'static str> {
    match state {
        AgentState::Idle | AgentState::Done | AgentState::Error => None,
        AgentState::Paused => Some("fork queued until the session is resumed"),
        _ => Some("fork queued until the turn ends"),
    }
}

/// `/fork [N]` → `Ok(None)` (everything) or `Ok(Some(N))`, else a usage error.
fn parse_fork_args(text: &str) -> Result<Option<u32>, String> {
    let rest = text.trim().strip_prefix("/fork").unwrap_or("").trim();
    if rest.is_empty() {
        return Ok(None);
    }
    rest.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("usage: /fork [<turn>] — `{rest}` is not a turn number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{OutEvent, SessionId};

    #[test]
    fn fork_args_parse_a_turn_or_nothing() {
        assert_eq!(parse_fork_args("/fork"), Ok(None));
        assert_eq!(parse_fork_args("/fork 2"), Ok(Some(2)));
        assert!(parse_fork_args("/fork two").is_err());
    }

    #[tokio::test]
    async fn fork_sends_fork_session_for_the_active_session() {
        let holly = Holly::spawn(entanglement_core::EngineConfig::default());
        let mut inbound = holly.subscribe_inbound();
        let mut app = App::new_for_test(SessionId::new("s1"));
        send_fork(&mut app, &holly, "/fork 3").await;
        let sent = inbound.recv().await.unwrap();
        assert!(matches!(
            sent,
            InMsg::ForkSession { session, at_turn: Some(3) } if session == SessionId::new("s1")
        ));
    }

    #[test]
    fn a_fork_sent_while_busy_or_paused_says_it_is_queued() {
        assert_eq!(deferral_note(AgentState::Done), None);
        assert_eq!(deferral_note(AgentState::Idle), None);
        assert!(
            deferral_note(AgentState::Thinking).is_some_and(|n| n.contains("until the turn ends"))
        );
        assert!(deferral_note(AgentState::WaitingApproval)
            .is_some_and(|n| n.contains("until the turn ends")));
        assert!(deferral_note(AgentState::Paused).is_some_and(|n| n.contains("resumed")));
    }

    #[tokio::test]
    async fn a_busy_session_records_the_queued_note() {
        let holly = Holly::spawn(entanglement_core::EngineConfig::default());
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(OutEvent::Status {
            session: SessionId::new("s1"),
            state: AgentState::Thinking,
        });
        send_fork(&mut app, &holly, "/fork").await;
        let view = app
            .sessions()
            .into_iter()
            .find(|(id, _)| id.0 == "s1")
            .map(|(_, v)| v.transcript().to_vec())
            .unwrap();
        assert!(view.iter().any(|e| matches!(
            e,
            crate::tui::session_view::TranscriptEntry::ToolOutput { output, .. }
                if output.contains("fork queued")
        )));
    }

    /// The fork of the watched session takes focus; a fork of some other
    /// session does not steal it.
    #[test]
    fn a_fork_of_the_active_session_becomes_active() {
        let forked = |session: &str, source: &str| OutEvent::SessionForked {
            session: SessionId::new(session),
            seq: 1,
            forked_from: SessionId::new(source),
            at_turn: None,
            context: Vec::new(),
        };
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_out_event(forked("bg-fork", "elsewhere"));
        assert_eq!(app.active_session_id(), &SessionId::new("s1"));
        app.handle_out_event(forked("f1", "s1"));
        assert_eq!(app.active_session_id(), &SessionId::new("f1"));
    }
}
