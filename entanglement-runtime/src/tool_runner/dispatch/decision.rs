//! Resolving a parked tool-approval decision (issue #451, split out of
//! `tool_runner.rs`): [`await_decision`] is `dispatch`'s tail once a call is
//! parked as `OutEvent::ToolRequest` — it waits for the head's
//! `Approve`/`Reject`/`Stop`, records a wider-scope grant on approval, and
//! either runs the call via [`super::execute::run_and_reply`] or replies with
//! a denial.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{AgentState, ApprovalScope, Holly, SessionId};

use crate::arg_validate;
use crate::hooks::Hooks;
use crate::policy::GrantStore;
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_advertising;
use crate::tools::ToolRegistry;

/// Park until the head answers the pending approval, then run-or-refuse. A
/// `Stop` (Esc-in-approval) unwinds silently: core's `wait_tool_result` sees the
/// same `Stop` on its inbox and cancels the turn, so no `ToolResult` is owed
/// (the shared park/filter is [`crate::seam::await_decision`]). `arg` is the
/// grading-time argument `dispatch` already computed (#485, ADR-0125) — taken
/// as a parameter rather than recomputed here, so the grant this records on
/// approval provably uses the exact same key `apply_grant` looked up before
/// the prompt was ever shown. `mode` is likewise threaded through so the
/// recorded grant is tagged with the mode it was actually approved under
/// (ADR-0207 §8). `question_timeout` (ADR-0207 §11, stage 5c) bounds the
/// park: `None` waits forever (every attended mode); `Some(d)` is the mode's
/// own `question_timeout`, and an elapsed wait expires as a denial —
/// silence is never consent for a privileged action.
#[allow(clippy::too_many_arguments)]
pub(super) async fn await_decision(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    grants: &dyn GrantStore,
    hooks: &Hooks,
    advertising: &tool_advertising::AdvertisingState,
    validation: &arg_validate::LoopBreaker,
    rx: tokio::sync::oneshot::Receiver<seam::Decision>,
    escape_grant: Option<(Arc<crate::extra_roots::ExtraRootStore>, std::path::PathBuf)>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
    arg: Option<String>,
    mode: String,
    question_timeout: Option<std::time::Duration>,
) {
    let decision = match crate::pending::await_decision_timed(rx, question_timeout).await {
        Some(decision) => decision,
        // Elapsed with no answer: `on_timeout` for a parked approval is
        // always `Deny` today (`crate::mode::OnTimeout`) — silence is never
        // consent for a privileged action (ADR-0207 §11).
        None => {
            set_thinking(holly, &session);
            let output = format!(
                "tool `{tool}` denied: no response within {}s (mode `{mode}`)",
                question_timeout.map(|d| d.as_secs()).unwrap_or_default()
            );
            seam::reply(holly, session, request_id, output, true).await;
            return;
        }
    };
    match decision {
        // `mode` (#560) is `propose_plan`-only; the generic permission `Ask`
        // approval this dispatch handles never sets it.
        seam::Decision::Approve { scope, .. } => {
            set_thinking(holly, &session);
            if let Some((store, abs)) = &escape_grant {
                // The prompt was forced by an out-of-root access (ADR-0109):
                // record the approval in the escape-root store so the host tool's
                // containment check lets *this tool* reach *this path*. Every scope
                // is recorded (a `Once` becomes the single-use token bound to this
                // exact `request_id`, #449, so a concurrent call to the same path
                // can't consume it); `Session`/`Always` also relax future
                // containment and let the executor skip re-asking. Per-tool by
                // construction.
                store.record(&tool, abs, scope, &request_id);
            } else if scope != ApprovalScope::Once {
                // Ordinary (in-root) approval: record the wider scopes (#174) so an
                // identical later call skips this prompt — through the pluggable
                // [`GrantStore`] (#311), tagged with the mode it was earned in
                // (ADR-0207 §8). `Once` records nothing.
                grants
                    .record(&session, &tool, arg.as_deref(), scope, &mode)
                    .await;
            }
            super::execute::run_and_reply(
                holly,
                tools,
                skills,
                active_skill,
                hooks,
                advertising,
                validation,
                session,
                request_id,
                tool,
                input,
            )
            .await;
        }
        seam::Decision::Reject { reason } => {
            set_thinking(holly, &session);
            let output = format!(
                "tool `{tool}` rejected: {}",
                reason.as_deref().unwrap_or("user")
            );
            seam::reply(holly, session, request_id, output, true).await;
        }
        // `Stop` (and a closed inbox) unwind silently; `Answer`/`Retract`/
        // `Replace` never target a tool-approval request id (they are
        // `ask_user`-only, #515).
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => {}
    }
}

fn set_thinking(holly: &Holly, session: &SessionId) {
    holly.emit_status(session, AgentState::Thinking);
}
