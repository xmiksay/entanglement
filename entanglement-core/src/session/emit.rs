//! Outbound-event emit helpers shared across the session loop and turn logic.
//! Each bumps the session's monotonic `seq` and fires an [`OutEvent`].

use tokio::sync::broadcast;

use crate::protocol::{AgentState, OutEvent, SessionId};

pub(crate) fn next_seq(s: &mut u64) -> u64 {
    *s += 1;
    *s
}

/// Surface a failed turn: an `Error`, a `Done` (so one-shot heads exit), then
/// the `Error` lifecycle state. The engine stays alive for the next prompt.
pub(crate) fn emit_turn_error(
    session: &SessionId,
    seq: &mut u64,
    events: &broadcast::Sender<OutEvent>,
    message: String,
) {
    let _ = events.send(OutEvent::Error {
        session: session.clone(),
        seq: next_seq(seq),
        message,
    });
    let _ = events.send(OutEvent::Done {
        session: session.clone(),
        seq: next_seq(seq),
    });
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Error,
    });
}

pub(crate) fn emit_tool_call(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    input: &str,
    seq: &mut u64,
) {
    let _ = events.send(OutEvent::ToolCall {
        session: session.clone(),
        seq: next_seq(seq),
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input: input.to_string(),
    });
}

pub(crate) fn emit_tool_output(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    output: String,
    seq: &mut u64,
) {
    let _ = events.send(OutEvent::ToolOutput {
        session: session.clone(),
        seq: next_seq(seq),
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        output,
    });
}
