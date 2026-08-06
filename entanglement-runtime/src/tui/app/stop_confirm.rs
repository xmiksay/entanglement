//! Cascade-vs-detach confirm for `Stop` on a plan session with a live
//! sponsored `propose_plan` build child (#626, ADR-0145 "Consequences").
//!
//! The backend already supports both: a `Stop` on the plan session alone
//! detaches (the build child keeps running, untouched — `propose_plan.rs`'s
//! `run_propose_plan` task simply never resumes); a second, explicit `Stop`
//! on the child cascades (ordinary `Stop` semantics, no special-casing). The
//! gap was purely interactive — the TUI had no way to *offer* the choice, so
//! it always detached. `SessionStarted.sponsored` (#626) now disambiguates
//! `AgentState::WaitingAgent`'s two callers — a plain blocking `agent`/
//! `agent_send` sub-agent wait has no sponsored child and is never offered
//! this choice, unlike a `propose_plan` handoff.
//!
//! This module owns only the *state*: whether the confirm is armed, and for
//! which (target, build_child) pair. The wire sends themselves live in
//! `crate::tui::stop_command`, which needs `Holly` — state here is plain
//! `App` data so every render/key-dispatch site can check it synchronously.

use entanglement_core::SessionId;

use super::App;

/// The plan session (`target`) and its live sponsored build child, armed by
/// `App::arm_stop_confirm` until the user picks cascade, detach, or cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopConfirm {
    pub target: SessionId,
    pub build_child: SessionId,
}

impl App {
    /// Whether `target` should offer a cascade-vs-detach choice before `Stop`
    /// rather than sending immediately: parked on `WaitingAgent` with a live
    /// sponsored build child. Returns the child to arm the confirm with.
    pub fn stop_needs_confirm(&self, target: &SessionId) -> Option<SessionId> {
        if self.sessions.state_of(target) != Some(entanglement_core::AgentState::WaitingAgent) {
            return None;
        }
        self.sessions.live_sponsored_child_of(target)
    }

    /// Arms the confirm prompt. The caller (`stop_command::request_stop`)
    /// sends no `Stop` yet — that happens once the user picks a resolution.
    pub fn arm_stop_confirm(&mut self, target: SessionId, build_child: SessionId) {
        self.pending_stop_confirm = Some(StopConfirm {
            target,
            build_child,
        });
        self.mark_dirty();
    }

    pub fn showing_stop_confirm(&self) -> bool {
        self.pending_stop_confirm.is_some()
    }

    pub fn stop_confirm(&self) -> Option<&StopConfirm> {
        self.pending_stop_confirm.as_ref()
    }

    /// Dismisses the confirm without sending anything (the `Esc`/`n` cancel
    /// path).
    pub fn clear_stop_confirm(&mut self) {
        if self.pending_stop_confirm.is_some() {
            self.pending_stop_confirm = None;
            self.mark_dirty();
        }
    }

    /// Takes the armed confirm so the caller can send the resolved `Stop`(s)
    /// exactly once.
    pub fn take_stop_confirm(&mut self) -> Option<StopConfirm> {
        let taken = self.pending_stop_confirm.take();
        if taken.is_some() {
            self.mark_dirty();
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use entanglement_core::{AgentState, OutEvent, SessionId};

    use crate::tui::app::App;

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
    fn waiting_agent_with_sponsored_child_needs_confirm() {
        let plan = SessionId::new("plan");
        let build = SessionId::new("build");
        let mut app = App::new_for_test(plan.clone());
        start_session(&mut app, &build, Some(&plan), true);
        set_state(&mut app, &plan, AgentState::WaitingAgent);

        assert_eq!(app.stop_needs_confirm(&plan), Some(build));
    }

    #[test]
    fn waiting_agent_with_a_plain_subagent_child_does_not_need_confirm() {
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut app = App::new_for_test(parent.clone());
        start_session(&mut app, &child, Some(&parent), false);
        set_state(&mut app, &parent, AgentState::WaitingAgent);

        assert_eq!(app.stop_needs_confirm(&parent), None);
    }

    #[test]
    fn a_session_not_parked_on_waiting_agent_never_needs_confirm() {
        let plan = SessionId::new("plan");
        let build = SessionId::new("build");
        let mut app = App::new_for_test(plan.clone());
        start_session(&mut app, &build, Some(&plan), true);
        set_state(&mut app, &plan, AgentState::Thinking);

        assert_eq!(app.stop_needs_confirm(&plan), None);
    }

    #[test]
    fn an_ended_sponsored_child_no_longer_needs_confirm() {
        let plan = SessionId::new("plan");
        let build = SessionId::new("build");
        let mut app = App::new_for_test(plan.clone());
        start_session(&mut app, &build, Some(&plan), true);
        set_state(&mut app, &plan, AgentState::WaitingAgent);
        app.handle_out_event(OutEvent::SessionEnded {
            session: build.clone(),
            ts: 2,
        });

        assert_eq!(app.stop_needs_confirm(&plan), None);
    }

    #[test]
    fn arm_take_and_clear_round_trip() {
        let plan = SessionId::new("plan");
        let build = SessionId::new("build");
        let mut app = App::new_for_test(plan.clone());
        assert!(!app.showing_stop_confirm());

        app.arm_stop_confirm(plan.clone(), build.clone());
        assert!(app.showing_stop_confirm());
        assert_eq!(
            app.stop_confirm().map(|c| c.target.clone()),
            Some(plan.clone())
        );

        let taken = app.take_stop_confirm().expect("armed");
        assert_eq!(taken.target, plan);
        assert_eq!(taken.build_child, build);
        assert!(!app.showing_stop_confirm());

        app.arm_stop_confirm(plan, build);
        app.clear_stop_confirm();
        assert!(!app.showing_stop_confirm());
    }
}
