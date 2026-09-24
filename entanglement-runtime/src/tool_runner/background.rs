//! The executor's long-lived background listeners (issue #712, split out of
//! `tool_runner.rs`): the passive plan-file `FileChange` listener and the
//! single inbound decision router. Both are parked in the executor's own
//! `JoinSet` so they're aborted when the executor task drops (#545).

use entanglement_core::{InMsg, OutEvent};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinSet;

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
/// (#607) directly from `open_questions`/the job+agent registries — read-only
/// snapshot queries, not decisions, so they don't go through `pending`.
/// One light map-lookup-per-frame loop drains far faster than a park
/// loop, so it does not lag the way the per-task subscriptions it
/// replaced did.
pub(super) fn spawn_decision_router(
    background: &mut JoinSet<()>,
    mut inbound: broadcast::Receiver<InMsg>,
    ctx: &LadderCtx,
) {
    let cancels = ctx.cancels.clone();
    let hooks = ctx.hooks.clone();
    let pending = ctx.pending.clone();
    let open_questions = ctx.open_questions.clone();
    let op_jobs = ctx.jobs.clone();
    let op_agents = ctx.registry.clone();
    let op_scripts = ctx.scripts.clone();
    let emitter = ctx.holly.clone();
    background.spawn(async move {
        loop {
            match inbound.recv().await {
                Ok(InMsg::Stop { session }) => {
                    cancels.cancel_session(&session);
                    pending.stop_session(&session);
                }
                Ok(InMsg::Prompt { session, content }) if !hooks.user_prompt_submit.is_empty() => {
                    // Detach so a slow hook can't stall the router.
                    let hooks = hooks.clone();
                    tokio::spawn(async move {
                        let text = entanglement_core::content_text(&content);
                        hooks.run_user_prompt_submit(&session, &text).await;
                    });
                }
                Ok(InMsg::ListQuestions {
                    correlation_id,
                    session,
                }) => {
                    let questions = open_questions.snapshot(session.as_ref());
                    emitter.emit_question_list(correlation_id, questions);
                }
                Ok(InMsg::ListOperations {
                    correlation_id,
                    session,
                }) => {
                    let operations = crate::operations::list_operations(
                        &op_jobs,
                        &op_agents,
                        &op_scripts,
                        session.as_ref(),
                    );
                    emitter.emit_operation_list(correlation_id, operations);
                }
                Ok(other) => {
                    if let Some((s, rid, decision)) = seam::Decision::from_inmsg(other) {
                        pending.resolve(&s, &rid, decision);
                    }
                }
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
