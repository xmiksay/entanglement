//! Tool-call dispatch within a turn: the built-in `update_plan`/`update_tasks`
//! handlers and the host-tool round-trip to the runtime (#58) — emit
//! `ToolExec`, park on `InMsg::ToolResult`, fold the output into context.

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_plan, emit_tasks, emit_tool_call, emit_tool_output, next_seq};
use super::{Session, SessionCmd, PLAN_TOOL, TASKS_TOOL};
use crate::llm::ToolCall;
use crate::protocol::{OutEvent, SessionId};

/// Dispatch one tool call. Returns `true` if the turn was cancelled.
pub(crate) async fn handle_tool_call(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    call: ToolCall,
) -> bool {
    emit_tool_call(
        events,
        session,
        &call.id,
        &call.name,
        &call.input,
        &mut s.seq,
    );

    // Built-ins: always run, mutate session state, emit a snapshot.
    if call.name == PLAN_TOOL {
        // Plan authority is default-closed (#140, ADR-0041). A non-owner should
        // never see the `update_plan` schema (it isn't advertised in `run_turn`),
        // but the model can still hallucinate the call — refuse it here with no
        // plan mutation and no `OutEvent::Plan`, and let the turn continue. This
        // must be caught in core: the built-ins never round-trip to the runtime,
        // so `tool_masked` cannot see them.
        if !s.profile.owns_plan {
            let msg = format!(
                "update_plan refused: the `{}` agent does not own the session plan; \
                 only a plan-owning profile may author it",
                s.profile.name
            );
            emit_tool_output(
                events,
                session,
                &call.id,
                PLAN_TOOL,
                msg.clone(),
                &mut s.seq,
            );
            s.ctx.push_tool(&call.id, msg);
            return false;
        }
        let plan = json_field(&call.input, "content").unwrap_or_else(|| call.input.clone());
        s.plan = plan;
        emit_plan(events, session, &s.plan, &mut s.seq);
        let msg = "plan updated".to_string();
        emit_tool_output(
            events,
            session,
            &call.id,
            PLAN_TOOL,
            msg.clone(),
            &mut s.seq,
        );
        s.ctx.push_tool(&call.id, msg.clone());
        tracing::debug!(tool_id = %call.id, result = %msg, "tool result pushed to context");
        return false;
    }
    if call.name == TASKS_TOOL {
        let tasks = json_field(&call.input, "content").unwrap_or_else(|| call.input.clone());
        s.tasks = tasks;
        emit_tasks(events, session, &s.tasks, &mut s.seq);
        let msg = "tasks updated".to_string();
        emit_tool_output(
            events,
            session,
            &call.id,
            TASKS_TOOL,
            msg.clone(),
            &mut s.seq,
        );
        s.ctx.push_tool(&call.id, msg);
        return false;
    }

    // Host tool: hand it to the runtime. Core no longer decides permission or
    // waits for approval (#59) — that policy moved to the runtime tool executor
    // (ADR-0003/0010), which resolves Allow/Ask/Deny, drives the approval UX,
    // and answers every call with `InMsg::ToolResult`. Core just emits the
    // request and parks on the result (the same #58 round-trip).
    run_tool_via_runtime(session, rx, s, events, stash, &call).await
}

/// Hand a permission-cleared tool call to the runtime and await its result
/// (#58). Emits [`OutEvent::ToolExec`], parks the turn on [`wait_tool_result`],
/// then surfaces the output as a [`OutEvent::ToolOutput`] and folds it into
/// context. Returns `true` if the turn was cancelled while waiting.
async fn run_tool_via_runtime(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    call: &ToolCall,
) -> bool {
    let _ = events.send(OutEvent::ToolExec {
        session: session.clone(),
        seq: next_seq(&mut s.seq),
        request_id: call.id.clone(),
        tool: call.name.clone(),
        input: call.input.clone(),
    });
    match wait_tool_result(rx, stash, &call.id).await {
        ToolResultOutcome::Ready(out) => {
            emit_tool_output(
                events,
                session,
                &call.id,
                &call.name,
                out.clone(),
                &mut s.seq,
            );
            s.ctx.push_tool(&call.id, out);
            false
        }
        ToolResultOutcome::Cancelled => true,
    }
}

enum ToolResultOutcome {
    Ready(String),
    Cancelled,
}

/// Wait for the runtime's [`SessionCmd::ToolResult`] matching `pending`,
/// stashing any other commands for replay after the turn. `Stop`/inbox-close
/// cancels the turn (ADR-0017); a late result for a cancelled call arrives at
/// the idle loop and is dropped as stale.
async fn wait_tool_result(
    rx: &mut mpsc::Receiver<SessionCmd>,
    stash: &mut VecDeque<SessionCmd>,
    pending: &str,
) -> ToolResultOutcome {
    loop {
        match rx.recv().await {
            Some(SessionCmd::ToolResult(id, output)) if id == pending => {
                return ToolResultOutcome::Ready(output)
            }
            Some(SessionCmd::Stop) | None => return ToolResultOutcome::Cancelled,
            Some(other) => stash.push_back(other),
        }
    }
}

/// Extract a field from a JSON-object tool input. Returns `None` when `input`
/// isn't a JSON object or lacks the field, so callers fall back to the raw
/// input — keeping scripted/test backends (raw strings) working alongside
/// structured providers (Anthropic sends a JSON object).
fn json_field(input: &str, field: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    match v.get(field) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) if !other.is_null() => Some(other.to_string()),
        _ => None,
    }
}
