//! `Stop` dispatch with the cascade-vs-detach confirm (#626, ADR-0145
//! "Consequences"): every interactive `Stop` site (bare `Esc`, `/stop`, the
//! sessions-modal `s` quick key, the command palette) routes a *single*
//! target through [`request_stop`] instead of sending `InMsg::Stop` directly,
//! so the choice is offered consistently everywhere. `/stop --all` fans out
//! raw `Stop`s and deliberately bypasses this — a bulk action confirming N
//! times would defeat its own purpose, and it keeps pre-#626 detach-always
//! semantics for that one form.

use anyhow::Result;
use entanglement_core::{Holly, InMsg, SessionId};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::app::{App, StopConfirm};

/// Send `Stop { session: target }`, arming the cascade-vs-detach confirm
/// instead when `target` is parked on `WaitingAgent` for a live sponsored
/// `propose_plan` build child (`App::stop_needs_confirm`). Otherwise sends
/// immediately — unchanged from pre-#626 behavior for every other case
/// (idle, `WaitingApproval`, a plain blocking `agent`/`agent_send` wait, ...).
pub(super) async fn request_stop(app: &mut App, holly: &Holly, target: SessionId) {
    if let Some(build_child) = app.stop_needs_confirm(&target) {
        app.arm_stop_confirm(target, build_child);
        return;
    }
    let _ = holly.send(InMsg::Stop { session: target }).await;
}

/// The `Stop` targets a resolved confirm sends: just the plan session for
/// detach, the plan session then its build child for cascade — pulled out as
/// a pure function so the resolution logic is testable without a live
/// `Holly`/turn to observe.
fn stop_targets(confirm: &StopConfirm, cascade: bool) -> Vec<SessionId> {
    let mut targets = vec![confirm.target.clone()];
    if cascade {
        targets.push(confirm.build_child.clone());
    }
    targets
}

/// Resolves the armed confirm: `cascade = true` also stops the build child.
async fn resolve_stop_confirm(app: &mut App, holly: &Holly, cascade: bool) {
    let Some(confirm) = app.take_stop_confirm() else {
        return;
    };
    for session in stop_targets(&confirm, cascade) {
        let _ = holly.send(InMsg::Stop { session }).await;
    }
}

/// Key handling while the confirm is armed: `c` cascades (stops the build
/// child too), `Enter`/`y`/`d` detaches (the pre-#626 default — leaves the
/// build running), `Esc`/`n` cancels and sends nothing.
pub(super) async fn handle_stop_confirm_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Char('c') => resolve_stop_confirm(app, holly, true).await,
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('d') => {
            resolve_stop_confirm(app, holly, false).await
        }
        KeyCode::Esc | KeyCode::Char('n') => app.clear_stop_confirm(),
        _ => {}
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use entanglement_core::{AgentState, EngineConfig, Holly, OutEvent, SessionId};
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn start_session(app: &mut App, id: &SessionId, parent: Option<&SessionId>, sponsored: bool) {
        app.handle_out_event(OutEvent::SessionStarted {
            session: id.clone(),
            parent: parent.cloned(),
            predecessor: None,
            profile: "build".to_string(),
            model: None,
            root: parent.is_none(),
            ts: 1,
            user: None,
            sponsored,
        });
    }

    fn set_state(app: &mut App, id: &SessionId, state: AgentState) {
        app.handle_out_event(OutEvent::Status {
            session: id.clone(),
            state,
        });
    }

    #[test]
    fn detach_targets_only_the_plan_session() {
        let confirm = StopConfirm {
            target: SessionId::new("plan"),
            build_child: SessionId::new("build"),
        };
        assert_eq!(stop_targets(&confirm, false), vec![SessionId::new("plan")]);
    }

    #[test]
    fn cascade_targets_the_plan_session_then_its_build_child() {
        let confirm = StopConfirm {
            target: SessionId::new("plan"),
            build_child: SessionId::new("build"),
        };
        assert_eq!(
            stop_targets(&confirm, true),
            vec![SessionId::new("plan"), SessionId::new("build")]
        );
    }

    #[tokio::test]
    async fn a_sponsored_wait_arms_the_confirm_instead_of_sending_stop() {
        let plan = SessionId::new("plan");
        let build = SessionId::new("build");
        let mut app = App::new_for_test(plan.clone());
        start_session(&mut app, &build, Some(&plan), true);
        set_state(&mut app, &plan, AgentState::WaitingAgent);

        let holly = Holly::spawn(EngineConfig::default());
        request_stop(&mut app, &holly, plan.clone()).await;

        assert!(app.showing_stop_confirm());
        let confirm = app.stop_confirm().expect("armed");
        assert_eq!(confirm.target, plan);
        assert_eq!(confirm.build_child, build);
    }

    #[tokio::test]
    async fn a_plain_waiting_agent_never_arms_the_confirm() {
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut app = App::new_for_test(parent.clone());
        start_session(&mut app, &child, Some(&parent), false);
        set_state(&mut app, &parent, AgentState::WaitingAgent);

        let holly = Holly::spawn(EngineConfig::default());
        request_stop(&mut app, &holly, parent.clone()).await;

        assert!(
            !app.showing_stop_confirm(),
            "a plain blocking sub-agent wait must never offer the cascade choice"
        );
    }

    #[tokio::test]
    async fn resolving_the_confirm_clears_it_and_sends_stop() {
        // Correct routing to `holly.send` is exercised end to end by the
        // `stop_on_the_plan_session_detaches_the_build_child_which_keeps_running`
        // / `stop_on_both_sessions_cascades_and_stops_the_build_child_too`
        // integration tests in `entanglement-runtime/tests/propose_plan.rs`,
        // which run the real production spawn + wait path; this only proves
        // the confirm state itself resolves (the `stop_targets` unit tests
        // above cover the cascade-vs-detach target selection).
        let plan = SessionId::new("plan");
        let build = SessionId::new("build");
        let mut app = App::new_for_test(plan.clone());
        app.arm_stop_confirm(plan, build);

        let holly = Holly::spawn(EngineConfig::default());
        resolve_stop_confirm(&mut app, &holly, true).await;

        assert!(!app.showing_stop_confirm());
    }

    #[tokio::test]
    async fn cancel_clears_the_confirm_and_sends_nothing() {
        let plan = SessionId::new("plan");
        let build = SessionId::new("build");
        let mut app = App::new_for_test(plan.clone());
        app.arm_stop_confirm(plan, build);

        let holly = Holly::spawn(EngineConfig::default());
        handle_stop_confirm_event(&mut app, &holly, key(KeyCode::Esc))
            .await
            .unwrap();

        assert!(!app.showing_stop_confirm());
    }
}
