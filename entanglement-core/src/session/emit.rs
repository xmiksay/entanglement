//! Outbound-event emit helpers shared across the session loop and turn logic.
//! Each bumps the session's monotonic `seq` and fires an [`OutEvent`].

use tokio::sync::broadcast;

use super::Session;
use crate::protocol::{AgentState, OutEvent, SessionId};
use entanglement_provider::Usage;

pub(crate) fn next_seq(s: &mut u64) -> u64 {
    *s += 1;
    *s
}

/// Fold one round-trip's normalized [`Usage`] into the session total and emit
/// the per-round-trip delta as [`OutEvent::Usage`] (#192). Missing dimensions
/// count as zero; `cost` is `None` when no catalog pricing covers the model.
pub(crate) fn emit_usage(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    usage: &Usage,
    cost: Option<f64>,
) {
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    let cached_input = usage.cached_input_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_tokens.unwrap_or(0);

    s.usage.input_tokens += input;
    s.usage.output_tokens += output;
    s.usage.cached_input_tokens += cached_input;
    s.usage.cache_write_tokens += cache_write;
    s.usage.cost_usd += cost.unwrap_or(0.0);

    let _ = events.send(OutEvent::Usage {
        session: session.clone(),
        seq: next_seq(&mut s.seq),
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached_input,
        cache_write_tokens: cache_write,
        cost_usd: cost,
    });
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
