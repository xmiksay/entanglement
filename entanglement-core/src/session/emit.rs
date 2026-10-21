//! Outbound-event emit helpers shared across the session loop and turn logic.
//! Each bumps the session's monotonic `seq` and fires an [`OutEvent`].

use tokio::sync::broadcast;

use super::{Session, TurnState};
use crate::holly::SeqCell;
use crate::protocol::{AgentState, OutEvent, SessionId, ToolEnvelope, UsagePurpose};
use entanglement_provider::{tool_reference_fallback_text, ContentPart, ImageSource, Usage};

// Every seq-bearing event goes out through `SeqCell::emit`, which mints the
// seq and broadcasts under the session's own lock (#893, ADR-0244): the
// runtime's `Holly::emit_for_session` takes the same lock, so delivery order
// is seq order across emitters.

/// Fold one round-trip's normalized [`Usage`] into the session total and emit
/// the per-round-trip delta as [`OutEvent::Usage`] (#192). Missing dimensions
/// count as zero; `cost` is `None` when no catalog pricing covers the model.
pub(crate) fn emit_usage(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    usage: &Usage,
    cost: Option<f64>,
    purpose: UsagePurpose,
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

    s.seq.emit(events, |seq| OutEvent::Usage {
        session: session.clone(),
        seq,
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached_input,
        cache_write_tokens: cache_write,
        cost_usd: cost,
        purpose,
    });
}

/// Surface a failed turn: an `Error`, a `Done` (so one-shot heads exit), then
/// the `Error` lifecycle state. The engine stays alive for the next prompt.
pub(crate) fn emit_turn_error(
    session: &SessionId,
    seq: &SeqCell,
    events: &broadcast::Sender<OutEvent>,
    message: String,
) {
    seq.emit(events, |seq| OutEvent::Error {
        session: session.clone(),
        seq,
        message,
    });
    seq.emit(events, |seq| OutEvent::Done {
        session: session.clone(),
        seq,
    });
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Error,
    });
}

/// Surface a completed turn: a `Done` (so one-shot heads exit), then the
/// `Done` lifecycle state. The sibling of [`emit_turn_error`] — every
/// termination path (confident stop, exhausted ambiguous-retry budget,
/// manual `/compact`) emits this exact pair, so a future change to how a turn
/// signals completion can't silently diverge between them (#434).
pub(crate) fn emit_turn_done(
    session: &SessionId,
    seq: &SeqCell,
    events: &broadcast::Sender<OutEvent>,
) {
    seq.emit(events, |seq| OutEvent::Done {
        session: session.clone(),
        seq,
    });
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Done,
    });
}

/// Display a tool call. `call` is the dispatch form; `envelope` is set when it
/// was unwrapped from an `mcp` envelope (ADR-0226).
pub(crate) fn emit_tool_call(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    call: &entanglement_provider::ToolCall,
    envelope: Option<&ToolEnvelope>,
    seq: &SeqCell,
) {
    seq.emit(events, |seq| OutEvent::ToolCall {
        session: session.clone(),
        seq,
        request_id: call.id.clone(),
        tool: call.name.clone(),
        input: call.input.clone(),
        provider_meta: call.provider_meta.clone(),
        envelope: envelope.cloned(),
    });
}

/// Hand a tool call to whoever executes it (#58): every host tool is a
/// protocol round-trip — the engine emits `ToolExec` and the runtime executor
/// (or any external resolver) answers with `InMsg::ToolResult`. `agent` is the
/// emitting session's active profile name, carried so the runtime resolves
/// permission/mask authoritatively rather than off its lossy lifecycle fold
/// (#156).
pub(crate) fn emit_tool_exec(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    call: &entanglement_provider::ToolCall,
    envelope: Option<&ToolEnvelope>,
    caller: Caller<'_>,
    seq: &SeqCell,
) {
    seq.emit(events, |seq| OutEvent::ToolExec {
        session: session.clone(),
        seq,
        request_id: call.id.clone(),
        tool: call.name.clone(),
        input: call.input.clone(),
        agent: caller.agent.to_string(),
        envelope: envelope.cloned(),
        workdir: caller.workdir.map(str::to_string),
    });
}

/// Who a `ToolExec` runs as: the session's agent (#156) and working directory
/// (#764) — both stamped on the call so the runtime never depends on a lossy
/// lifecycle fold to know them.
#[derive(Clone, Copy)]
pub(crate) struct Caller<'a> {
    pub agent: &'a str,
    pub workdir: Option<&'a str>,
}

impl<'a> Caller<'a> {
    pub(crate) fn of(s: &'a super::Session) -> Self {
        Self {
            agent: &s.agent.name,
            workdir: s.workdir.as_deref(),
        }
    }
}

/// Re-offer every pending call of a parked batch — same `request_id`, fresh
/// `seq` — with the envelope its first offer carried. Shared by the
/// resume-mid-turn path and the re-offer timer (ADR-0061/0071).
pub(crate) fn reoffer_pending(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    turn: &TurnState,
    caller: Caller<'_>,
    seq: &SeqCell,
) {
    for call in &turn.pending {
        emit_tool_exec(
            events,
            session,
            call,
            turn.envelopes.get(&call.id),
            caller,
            seq,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_tool_output(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    content: Vec<ContentPart>,
    is_error: bool,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
    envelope: Option<ToolEnvelope>,
    seq: &SeqCell,
) {
    // Heads render text; an image result shows a short placeholder. The full
    // multimodal `content` rides whenever `output` can't rebuild it exactly —
    // an image (#221), a tool reference, several parts — so replay pushes the
    // same result the live context holds (ADR-0202), while the common single
    // text case stays a bare `output` string (no duplicated array in the log).
    let rebuildable = match content.as_slice() {
        [] => true,
        [ContentPart::Text { text }] => !text.is_empty(),
        _ => false,
    };
    let output = tool_output_display(&content);
    seq.emit(events, |seq| OutEvent::ToolOutput {
        session: session.clone(),
        seq,
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        output,
        content: if rebuildable { Vec::new() } else { content },
        is_error,
        duration_ms,
        exit_code,
        envelope,
    });
}

/// The text a head displays for a tool result: text parts verbatim, each image
/// part as a compact `[image: <media_type>]` placeholder (its base64 is useless
/// on a terminal), each provider-search block (#481) as its `summary`.
fn tool_output_display(content: &[ContentPart]) -> String {
    content
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => text.clone(),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, .. },
            } => format!("[image: {media_type}]"),
            ContentPart::ProviderSearch { summary, .. } => summary.clone(),
            // A tool result never carries reasoning — the model authors it, the
            // tool executor does not — but the match is exhaustive, and showing
            // nothing is the right answer if one ever appeared.
            ContentPart::Reasoning { .. } => String::new(),
            // Legacy-only: a pre-ADR-0226 log's discovery replies carry these
            // (nothing mints them now); show the same portable text every
            // wire's request converter falls back to.
            ContentPart::ToolReference { tool_name } => tool_reference_fallback_text(tool_name),
            ContentPart::ToolSearchOutput { summary, .. } => summary.clone(),
        })
        .collect()
}

/// Emit a persisted `SearchResult` content event (#481) for one provider-side
/// web-search block minted this round, so `Session::replay` can reconstruct
/// the assistant message's content — including citations and (Anthropic) the
/// search-cache pricing benefit of replaying the block verbatim — instead of
/// it vanishing with the round like the pre-#481 `Reasoning`-only channel.
pub(crate) fn emit_search_result(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    part: ContentPart,
    seq: &SeqCell,
) {
    seq.emit(events, |seq| OutEvent::SearchResult {
        session: session.clone(),
        seq,
        part,
    });
}

/// Emit a persisted `ReasoningBlock` content event for one extended-thinking
/// block captured this round, so `Session::replay` can reconstruct the assistant
/// message's content verbatim. Without it a resumed parked turn would rebuild a
/// history whose final assistant message is missing the block Anthropic requires
/// alongside tool results, and the continuation request would 400.
///
/// Capture is unconditional and so is this event; whether the block is actually
/// replayed to the provider is decided later, per model, by
/// `ModelEntry::replay_thinking`.
pub(crate) fn emit_reasoning_block(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    part: ContentPart,
    seq: &SeqCell,
) {
    seq.emit(events, |seq| OutEvent::ReasoningBlock {
        session: session.clone(),
        seq,
        part,
    });
}
