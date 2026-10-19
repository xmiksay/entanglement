//! `Stop` dispatch (#6): the single entry point every interactive `Stop` site
//! (bare `Esc`, approval/question `Esc`, `/stop [--all]`, the sessions-modal
//! `s` quick key, the command palette) routes a target through, so there is
//! one place that sends the `InMsg` and clears the view's cards.
//!
//! Sessions used to offer a cascade-vs-detach confirm here for a
//! `propose_plan` sponsored build child (#626, ADR-0145). ADR-0207 §7
//! retires that handoff entirely — approving a plan switches the session's
//! own mode instead of spawning a child — so the confirm's premise is gone
//! and `Stop` now always sends immediately.

use entanglement_core::{Holly, InMsg, SessionId};

use super::app::App;

/// Send `Stop { session: target }` immediately, then clear `target`'s
/// approval cards. The turn's own cards would go with its terminal
/// `Status`, but a background script's survive that (ADR-0237) — and the
/// runtime's `stop_session` unwinds them too, so a card left shown would
/// answer nothing.
pub(super) async fn request_stop(app: &mut App, holly: &Holly, target: SessionId) {
    let _ = holly
        .send(InMsg::Stop {
            session: target.clone(),
        })
        .await;
    app.clear_approvals_of(&target);
}

#[cfg(test)]
mod tests {
    use entanglement_core::{EngineConfig, OutEvent};

    use super::*;

    fn detached_card(session: &SessionId) -> OutEvent {
        OutEvent::ToolRequest {
            session: session.clone(),
            seq: 1,
            request_id: "bg".into(),
            tool: "edit (rhai)".into(),
            input: "{}".into(),
            reason: None,
            scopes: Vec::new(),
            detached: true,
        }
    }

    fn cards(app: &App, session: &SessionId) -> usize {
        app.sessions()
            .into_iter()
            .find(|(id, _)| *id == session)
            .and_then(|(_, v)| v.pending_tool_request())
            .map_or(0, |_| 1)
    }

    /// The runtime's `stop_session` unwinds a background script's card, so
    /// every Stop site — not only approval-mode `Esc` — must clear it, on
    /// the active session and a background one alike (ADR-0237).
    #[tokio::test]
    async fn a_stop_clears_the_targets_detached_card() {
        let holly = Holly::spawn(EngineConfig::default());
        let first = SessionId::new("first");
        let mut app = App::new_for_test(first.clone());
        let active = app.create_session();
        for s in [&first, &active] {
            app.handle_out_event(detached_card(s));
            assert_eq!(cards(&app, s), 1);
        }

        request_stop(&mut app, &holly, first.clone()).await;
        assert_eq!(cards(&app, &first), 0, "a background session's card");
        assert_eq!(cards(&app, &active), 1, "another session is untouched");

        request_stop(&mut app, &holly, active.clone()).await;
        assert_eq!(cards(&app, &active), 0, "the active session's card");
        assert!(matches!(
            app.approval_mode(),
            crate::tui::session_view::ApprovalMode::Normal
        ));
    }
}
