//! Server-Sent-Events parsing for the OpenAI Responses API streaming wire.
//! Split out of `openai_responses/mod.rs` (mirrors `openai/sse.rs`) to keep
//! the streaming client itself under the file-size cap.
//!
//! Unlike Chat Completions' single per-stream `BTreeMap<index, PendingTool>`
//! assembly, a Responses item is self-contained: `response.output_item.done`
//! always carries the item's complete, final JSON (id, name, arguments, …),
//! so a tool call is read out in one place with no cross-chunk accumulation
//! state. This client deliberately does not forward
//! [`LlmEvent::ToolCallDelta`] (the incremental-argument event) — a
//! `response.function_call_arguments.delta` chunk carries only `item_id` and
//! `delta`, no `name`/`call_id`, and correlating it to those would need
//! `response.output_item.added`'s exact field list, which
//! `scratch/tool-search-wire-reference.md` §2.6 flags **UNVERIFIED** (seen
//! only via an indirect fetch). `ToolCallDelta` is documented as additive —
//! "a consumer that only needs the assembled call can ignore it" — so
//! skipping it here costs only live per-keystroke argument rendering, not
//! correctness.

use crate::{LlmEvent, StopReason, ToolCall, Usage};
use serde_json::Value;

use super::TOOL_SEARCH_CALL_TOOL;

/// Per-stream state `handle_event` folds into: whether any tool call (real or
/// `tool_search_call`) was emitted this turn (decides the `Finish` stop
/// reason, ADR-0118), and whether a terminal event
/// (`response.completed`/`.incomplete`) was already seen — `mod.rs`'s outer
/// loop uses this to tell "the stream reported its own end" from "the
/// connection just closed" and skips a duplicate `Finish`.
#[derive(Default)]
pub(super) struct StreamState {
    pub emitted_any_tool_call: bool,
    pub terminated: bool,
}

enum SseEvent {
    Skip,
    Data(Value),
}

/// Parse one already-delimited SSE line. Responses API frames carry a
/// `data: <json>` line (optionally preceded by an `event: <type>` line this
/// parser ignores — the payload's own `"type"` field, confirmed in every
/// example JSON in the wire reference, carries the same information); unlike
/// Chat Completions there is no `data: [DONE]` terminator (the connection
/// closes after `response.completed`/`.failed`), but a literal `[DONE]` is
/// still tolerated as a no-op rather than a parse error, defensively.
fn parse_sse_line(line: &str) -> SseEvent {
    let line = line.trim();
    if line.is_empty() {
        return SseEvent::Skip;
    }
    let Some(payload) = line.strip_prefix("data:") else {
        return SseEvent::Skip;
    };
    let payload = payload.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return SseEvent::Skip;
    }
    match serde_json::from_str(payload) {
        Ok(v) => SseEvent::Data(v),
        Err(_) => SseEvent::Skip,
    }
}

/// Drain every complete frame currently buffered, folding into `state`/`usage`
/// and collecting events. Returns `(events, terminated)` — once a terminal
/// event is handled, stops draining immediately, mirroring the Chat
/// Completions client's `[DONE]` early-break.
pub(super) fn drain_available_frames(
    frames: &mut crate::sse_frame::SseFrameBuffer,
    state: &mut StreamState,
    usage: &mut Usage,
) -> anyhow::Result<(Vec<LlmEvent>, bool)> {
    let mut out = Vec::new();
    while let Some(line) = frames.next_frame() {
        if let SseEvent::Data(data) = parse_sse_line(&line) {
            if handle_event(&data, state, usage, &mut out)? {
                return Ok((out, true));
            }
        }
    }
    Ok((out, false))
}

/// Fold one parsed `data:` payload. Returns whether the stream is now
/// complete. Every field read here is `Option`-shaped with a safe fallback —
/// an unrecognized `"type"`, or a recognized one missing an expected field,
/// is ignored rather than erroring (tolerant parse per the wire reference's
/// UNVERIFIED flags on this API's exact event shapes, §2.6-2.7).
fn handle_event(
    data: &Value,
    state: &mut StreamState,
    usage: &mut Usage,
    out: &mut Vec<LlmEvent>,
) -> anyhow::Result<bool> {
    let Some(kind) = data.get("type").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    match kind {
        "response.output_text.delta" => {
            if let Some(text) = data.get("delta").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    out.push(LlmEvent::Text(text.to_string()));
                }
            }
        }
        "response.output_item.done" => {
            if let Some(item) = data.get("item") {
                if let Some(call) = tool_call_from_item(item) {
                    state.emitted_any_tool_call = true;
                    out.push(LlmEvent::ToolCall(call));
                }
            }
        }
        "response.completed" => {
            note_usage(data.pointer("/response/usage"), usage);
            state.terminated = true;
            let stop_reason = Some(if state.emitted_any_tool_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            });
            out.push(LlmEvent::Finish {
                stop_reason,
                usage: *usage,
            });
            return Ok(true);
        }
        // Not in the wire reference's confirmed event list (§2.6) but a
        // well-established Responses API terminal state (output cut short by
        // `max_output_tokens` or a content filter) — handled defensively,
        // same tolerant-parse posture as every other branch here.
        "response.incomplete" => {
            note_usage(data.pointer("/response/usage"), usage);
            state.terminated = true;
            let reason = data
                .pointer("/response/incomplete_details/reason")
                .and_then(|v| v.as_str());
            let stop_reason = Some(match reason {
                Some("max_output_tokens") => StopReason::MaxTokens,
                _ => StopReason::Other,
            });
            out.push(LlmEvent::Finish {
                stop_reason,
                usage: *usage,
            });
            return Ok(true);
        }
        "response.failed" => {
            let message = data
                .pointer("/response/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("openai-responses stream failed: {message}");
        }
        "error" => {
            let message = data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("openai-responses stream error: {message}");
        }
        _ => {}
    }
    Ok(false)
}

fn note_usage(value: Option<&Value>, usage: &mut Usage) {
    let Some(u) = value else { return };
    // Mirrors the Chat Completions client's cached/uncached split (#192) —
    // `input_tokens` here already includes cached reads, same as OpenAI's
    // `prompt_tokens`.
    let cached = u
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64());
    if let Some(c) = cached {
        usage.cached_input_tokens = Some(c);
    }
    if let Some(p) = u.get("input_tokens").and_then(|v| v.as_u64()) {
        usage.input_tokens = Some(p.saturating_sub(cached.unwrap_or(0)));
    }
    if let Some(c) = u.get("output_tokens").and_then(|v| v.as_u64()) {
        usage.output_tokens = Some(c);
    }
}

/// Map a completed output item to a [`ToolCall`]: a `function_call` (a real
/// tool) or a `tool_search_call` (client-execution mode only — mapped onto
/// the reserved [`TOOL_SEARCH_CALL_TOOL`] name so it rides the existing
/// `ToolCall`/`ToolExec` machinery). A hosted-mode `tool_search_call`
/// (`execution: "server"`, `call_id: null`) is skipped — this client only
/// ever declares client execution (`request::tool_search_entry`), so seeing
/// one would mean the server ignored that, and there is no `call_id` to
/// correlate a reply against anyway. `message`/`reasoning`/anything else
/// returns `None`.
fn tool_call_from_item(item: &Value) -> Option<ToolCall> {
    let kind = item.get("type").and_then(|v| v.as_str())?;
    match kind {
        "function_call" => {
            let call_id = item.get("call_id").and_then(|v| v.as_str())?.to_string();
            let name = item.get("name").and_then(|v| v.as_str())?.to_string();
            let arguments = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}")
                .to_string();
            Some(ToolCall::new(call_id, name, arguments))
        }
        "tool_search_call" => {
            let execution = item.get("execution").and_then(|v| v.as_str());
            if execution != Some("client") {
                return None;
            }
            let call_id = item.get("call_id").and_then(|v| v.as_str())?.to_string();
            let arguments = item
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            Some(ToolCall::new(
                call_id,
                TOOL_SEARCH_CALL_TOOL,
                arguments.to_string(),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_of(lines: &[&str]) -> crate::sse_frame::SseFrameBuffer {
        let mut f = crate::sse_frame::SseFrameBuffer::new(b"\n");
        for line in lines {
            f.push(format!("{line}\n").as_bytes());
        }
        f
    }

    #[test]
    fn text_delta_emits_llm_event_text() {
        let mut frames =
            frames_of(&[r#"data: {"type":"response.output_text.delta","delta":"hi"}"#]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, done) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert_eq!(events, vec![LlmEvent::Text("hi".to_string())]);
        assert!(!done);
    }

    #[test]
    fn function_call_item_emits_tool_call() {
        let mut frames = frames_of(&[
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}"#,
        ]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, _) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                r#"{"city":"Paris"}"#
            ))]
        );
        assert!(state.emitted_any_tool_call);
    }

    #[test]
    fn client_tool_search_call_maps_to_reserved_name() {
        let mut frames = frames_of(&[
            r#"data: {"type":"response.output_item.done","item":{"type":"tool_search_call","execution":"client","call_id":"call_2","status":"completed","arguments":{"query":"weather"}}}"#,
        ]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, _) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        let LlmEvent::ToolCall(call) = &events[0] else {
            panic!("expected a ToolCall event: {events:?}");
        };
        assert_eq!(call.id, "call_2");
        assert_eq!(call.name, TOOL_SEARCH_CALL_TOOL);
        assert_eq!(call.input, r#"{"query":"weather"}"#);
    }

    #[test]
    fn hosted_tool_search_call_is_ignored() {
        let mut frames = frames_of(&[
            r#"data: {"type":"response.output_item.done","item":{"type":"tool_search_call","execution":"server","call_id":null,"arguments":{"paths":["crm"]}}}"#,
        ]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, _) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert!(events.is_empty());
        assert!(!state.emitted_any_tool_call);
    }

    #[test]
    fn completed_carries_usage_and_confident_stop_reason() {
        let mut frames = frames_of(&[
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":20},"output_tokens":30}}}"#,
        ]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, done) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert!(done);
        assert!(state.terminated);
        let LlmEvent::Finish { stop_reason, usage } = &events[0] else {
            panic!("expected Finish: {events:?}");
        };
        assert_eq!(*stop_reason, Some(StopReason::EndTurn));
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.cached_input_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(30));
    }

    #[test]
    fn completed_after_a_tool_call_reports_tool_use() {
        let mut frames = frames_of(&[
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read","arguments":"{}"}}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, done) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert!(done);
        let LlmEvent::Finish { stop_reason, .. } = events.last().unwrap() else {
            panic!("expected Finish last: {events:?}");
        };
        assert_eq!(*stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn incomplete_max_output_tokens_maps_to_max_tokens() {
        let mut frames = frames_of(&[
            r#"data: {"type":"response.incomplete","response":{"usage":{"input_tokens":5,"output_tokens":5},"incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, done) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert!(done);
        let LlmEvent::Finish { stop_reason, .. } = &events[0] else {
            panic!("expected Finish: {events:?}");
        };
        assert_eq!(*stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn failed_event_bails() {
        let mut frames = frames_of(&[
            r#"data: {"type":"response.failed","response":{"error":{"message":"boom"}}}"#,
        ]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let err = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn unknown_event_type_is_skipped_tolerantly() {
        let mut frames = frames_of(&[r#"data: {"type":"response.file_search_call.in_progress"}"#]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, done) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert!(events.is_empty());
        assert!(!done);
    }

    #[test]
    fn malformed_json_line_is_skipped() {
        let mut frames = frames_of(&["data: not json"]);
        let mut state = StreamState::default();
        let mut usage = Usage::default();
        let (events, done) = drain_available_frames(&mut frames, &mut state, &mut usage).unwrap();
        assert!(events.is_empty());
        assert!(!done);
    }
}
