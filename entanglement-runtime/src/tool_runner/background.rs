//! The executor's long-lived background listeners (issue #712, split out of
//! `tool_runner.rs`): the passive plan-file `FileChange` listener and the
//! single inbound decision router. Both are parked in the executor's own
//! `JoinSet` so they're aborted when the executor task drops (#545).

use entanglement_core::{InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinSet;

use crate::pending::Resolution;
use crate::seam;

use super::ladder::LadderCtx;

/// Per-session plan-file staleness tracking (#513): the registry passed in to
/// the executor, kept fresh by `propose_plan` itself, passively by this
/// `FileChange` listener, and — out of band — by
/// `plan_watch::spawn_plans_watcher` if the caller wired one up against this
/// same instance (#627).
pub(super) fn spawn_file_change_listener(background: &mut JoinSet<()>, ctx: &LadderCtx) {
    let mut file_changes = ctx.holly.subscribe();
    let plan_files = ctx.plan_files.clone();
    let plan_root = ctx.plan_root.clone();
    background.spawn(async move {
        loop {
            match file_changes.recv().await {
                Ok(OutEvent::FileChange {
                    session,
                    path,
                    hash,
                    ..
                }) => {
                    let rel = crate::permission_path::rooted_arg(&plan_root, "write", &path);
                    plan_files.note_file_change(&session, &rel, &hash);
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    // Best-effort: a missed `FileChange` just means the
                    // registry's next staleness check treats an
                    // in-session edit as if it were external — fails
                    // closed (asks the agent to re-read), never open.
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// The single inbound router (#156): the *sole* consumer of the inbound
/// fan-out for decisions. It watches `Stop` (cancel in-flight tools +
/// unwind parked approvals, #167), fires the `user_prompt_submit` hooks
/// (#199) off each `Prompt`, resolves every `Approve`/`Reject`/
/// `Answer`/`RetractQuestion`/`ReplaceQuestion` (#515) to its parked
/// waiter, and answers `InMsg::ListQuestions` (#515)/`InMsg::ListOperations`
/// (#607)/`InMsg::ListPending` (#726) directly from `open_questions`/the
/// job+agent registries/`pending` — read-only snapshot queries, not decisions.
/// One light map-lookup-per-frame loop drains far faster than a park
/// loop, so it does not lag the way the per-task subscriptions it
/// replaced did.
pub(super) fn spawn_decision_router(
    background: &mut JoinSet<()>,
    mut inbound: broadcast::Receiver<InMsg>,
    ctx: &LadderCtx,
) {
    let router = Router {
        cancels: ctx.cancels.clone(),
        hooks: ctx.hooks.clone(),
        pending: ctx.pending.clone(),
        open_questions: ctx.open_questions.clone(),
        jobs: ctx.jobs.clone(),
        agents: ctx.registry.clone(),
        scripts: ctx.scripts.clone(),
        holly: ctx.holly.clone(),
    };
    background.spawn(async move {
        loop {
            match inbound.recv().await {
                Ok(msg) => router.route(msg),
                // A lagging router would strand a decision; warn loudly.
                // In practice this loop can't fall behind the inbound fill
                // rate — this is not the #156 failure mode it fixes.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        "decision router lagged; some inbound frames dropped"
                    );
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// What the router reads and writes: every registry a frame it handles
/// touches.
pub(super) struct Router {
    pub(super) cancels: crate::cancel::CancelRegistry,
    pub(super) hooks: std::sync::Arc<crate::hooks::Hooks>,
    pub(super) pending: crate::pending::PendingDecisions,
    pub(super) open_questions: crate::questions::OpenQuestions,
    pub(super) jobs: crate::host::jobs::JobRegistry,
    pub(super) agents: crate::agent_registry::AgentRegistry,
    pub(super) scripts: crate::script_ops::ScriptRegistry,
    pub(super) holly: entanglement_core::Holly,
}

impl Router {
    /// Handle one inbound frame. Synchronous: a slow hook is detached.
    pub(super) fn route(&self, msg: InMsg) {
        // A compaction successor's cards a lagged fold never re-homed
        // (ADR-0238), before anything reads, answers or stops them.
        crate::pending::heal_before(&self.holly, &self.pending, &msg);
        match msg {
            InMsg::Stop { session } => {
                self.cancels.cancel_session(&session);
                self.pending.stop_session(&session);
            }
            InMsg::Prompt { session, content } if !self.hooks.user_prompt_submit.is_empty() => {
                // Detach so a slow hook can't stall the router.
                let hooks = self.hooks.clone();
                tokio::spawn(async move {
                    let text = entanglement_core::content_text(&content);
                    hooks.run_user_prompt_submit(&session, &text).await;
                });
            }
            InMsg::ListQuestions {
                correlation_id,
                session,
            } => {
                let questions = self.open_questions.snapshot(session.as_ref());
                self.holly.emit_question_list(correlation_id, questions);
            }
            InMsg::ListOperations {
                correlation_id,
                session,
            } => {
                self.agents.heal_from(&self.holly);
                let operations = crate::operations::list_operations(
                    &self.jobs,
                    &self.agents,
                    &self.scripts,
                    session.as_ref(),
                );
                self.holly.emit_operation_list(correlation_id, operations);
            }
            // In order with `Approve`/`Reject` on this same loop, so a list
            // after an answer never still shows the answered call.
            InMsg::ListPending {
                correlation_id,
                root,
            } => {
                // The `root` walk follows re-parented and compacted
                // sub-agents only once the registry is healed (ADR-0234),
                // and lists their scripts' cards once moved (ADR-0238).
                self.agents.heal_from(&self.holly);
                if let Some(root) = &root {
                    crate::pending::heal_subtree(&self.holly, &self.pending, &self.agents, root);
                }
                let approvals =
                    crate::pending::list_pending(&self.pending, &self.agents, root.as_ref());
                self.holly.emit_pending_list(correlation_id, approvals);
            }
            other => {
                if let Some((s, rid, decision)) = seam::Decision::from_inmsg(other) {
                    let (frame, question) = (decision.frame(), decision.is_question());
                    if let Resolution::Misaimed { kind } = self.pending.resolve(&s, &rid, decision)
                    {
                        // Misaimed: the card is the kind the frame is not.
                        self.refuse_misaimed(&s, &rid, frame, kind, !question);
                    }
                }
            }
        }
    }

    /// A decision of the wrong kind for its card (ADR-0245): the card stays
    /// parked, and the sender learns why on the session it addressed.
    /// Unwinding it instead would owe the model a `ToolResult` nobody sends.
    fn refuse_misaimed(
        &self,
        session: &SessionId,
        request_id: &str,
        frame: &str,
        kind: &str,
        card_is_question: bool,
    ) {
        let message = if card_is_question {
            format!(
                "`{frame}` refused: `{request_id}` is an `{kind}` question; answer it with \
                 `AnswerQuestion`, `RetractQuestion` or `ReplaceQuestion`"
            )
        } else {
            format!(
                "`{frame}` refused: `{request_id}` is an approval card ({kind}), not an \
                 `ask_user` question; answer it with `Approve` or `Reject`"
            )
        };
        tracing::warn!(%session, request_id, frame, kind, "misaimed decision refused");
        self.holly.emit_for_session(session, |seq| OutEvent::Error {
            session: session.clone(),
            seq,
            message,
        });
    }
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "router_misaimed_tests.rs"]
mod misaimed_tests;
