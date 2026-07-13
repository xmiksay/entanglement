//! Tool-call dispatch within a turn: the host-tool round-trip to the runtime
//! (#58) — emit `ToolExec`, park on `InMsg::ToolResult`, fold the output into
//! context. Every tool takes this path, including the runtime's
//! `update_plan`/`update_tasks` state tools (#231, ADR-0049); core no longer has
//! built-ins.

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_tool_call, emit_tool_output, next_seq};
use super::{Session, SessionCmd};
use crate::protocol::{OutEvent, SessionId};
use entanglement_provider::ToolCall;

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

    // Every tool is a runtime round-trip: core no longer decides permission or
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
