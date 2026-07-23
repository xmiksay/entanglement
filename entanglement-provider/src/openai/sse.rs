//! Server-Sent-Events chunk handling for the OpenAI Chat Completions streaming
//! wire. Split out of `openai/mod.rs` (#481) to keep the streaming client
//! itself under the file-size cap.

use std::collections::BTreeMap;

use crate::{ContentPart, LlmEvent, Usage};
use serde_json::Value;

use super::PendingTool;

/// The outcome of parsing one already-delimited SSE line.
pub(super) enum SseEvent {
    /// `data: [DONE]` — the protocol-correct terminator (#483). The caller
    /// must stop reading the stream, ignoring anything buffered afterward.
    Done,
    /// A blank line, a non-`data:` line, or a payload that fails to parse as
    /// JSON — ignored rather than erroring, matching the pre-#483 behavior.
    Skip,
    /// A parsed JSON chunk ready for `handle_chunk`.
    Data(Value),
}

pub(super) fn parse_sse_line(line: &str) -> SseEvent {
    let line = line.trim();
    if line.is_empty() {
        return SseEvent::Skip;
    }
    let Some(payload) = line.strip_prefix("data:") else {
        return SseEvent::Skip;
    };
    let payload = payload.trim();
    if payload == "[DONE]" {
        return SseEvent::Done;
    }
    match serde_json::from_str(payload) {
        Ok(v) => SseEvent::Data(v),
        Err(_) => SseEvent::Skip,
    }
}

pub(super) fn note_finish_reason(data: &Value, seen: &mut Option<String>) {
    if let Some(fr) = data
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
    {
        *seen = Some(fr.to_string());
    }
}

/// Drain every complete frame currently buffered in `frames`, updating
/// `tools`/`usage`/`seen_finish_reason` and collecting events to yield.
/// Returns `(events, saw_done)` — once `[DONE]` is seen, stops immediately
/// without draining any further frames still sitting in the buffer, so the
/// caller can tell "the stream is over" from "there's more to flush at EOF".
/// Pulled out of `stream()`'s `try_stream!` block so it is plain, synchronous,
/// and unit-testable (#483).
pub(super) fn drain_available_frames(
    frames: &mut crate::sse_frame::SseFrameBuffer,
    tools: &mut BTreeMap<u32, PendingTool>,
    usage: &mut Usage,
    seen_finish_reason: &mut Option<String>,
) -> anyhow::Result<(Vec<LlmEvent>, bool)> {
    let mut out = Vec::new();
    while let Some(line) = frames.next_frame() {
        match parse_sse_line(&line) {
            SseEvent::Done => return Ok((out, true)),
            SseEvent::Skip => {}
            SseEvent::Data(data) => {
                note_finish_reason(&data, seen_finish_reason);
                out.extend(handle_chunk(&data, tools, usage)?);
            }
        }
    }
    Ok((out, false))
}

/// Map one parsed `data:` chunk to zero or more [`LlmEvent`]s, updating tool
/// assembly + usage state. Pure (no I/O) so it unit-tests directly. Tool calls
/// are *not* flushed here even once `finish_reason == "tool_calls"` is seen —
/// the caller keeps assembling in `tools` and flushes once, with JSON
/// validation, after the stream ends (`flush_pending_tools`) so there is a
/// single validating flush path instead of two that can disagree (#445).
pub(super) fn handle_chunk(
    data: &Value,
    tools: &mut BTreeMap<u32, PendingTool>,
    usage: &mut Usage,
) -> Result<Vec<LlmEvent>, anyhow::Error> {
    let mut out = Vec::new();

    // Usage arrives in the final chunk (empty choices when include_usage is set).
    if let Some(u) = data.get("usage") {
        // OpenAI's `prompt_tokens` *includes* cache-read tokens; split them so
        // each maps to one pricing dimension (#192): the cached portion bills at
        // the cached rate, the remainder at the full input rate.
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|v| v.as_u64());
        if let Some(c) = cached {
            usage.cached_input_tokens = Some(c);
        }
        if let Some(p) = u.get("prompt_tokens").and_then(|v| v.as_u64()) {
            usage.input_tokens = Some(p.saturating_sub(cached.unwrap_or(0)));
        }
        if let Some(c) = u.get("completion_tokens").and_then(|v| v.as_u64()) {
            usage.output_tokens = Some(c);
        }
    }

    // Provider-side web search (#305): z.ai streams the source list as a
    // `web_search` array. Its exact placement in the streaming wire is unverified,
    // so scan defensively at the top level and inside the delta; each source
    // surfaces as one Reasoning line (cited answer text already flows as `Text`),
    // plus one persisted `ContentBlock` (#481) so citations survive into a later
    // turn's history instead of vanishing with the round.
    emit_web_search(data.get("web_search"), &mut out);
    if let Some(arr) = data.pointer("/choices/0/delta/web_search") {
        emit_web_search(Some(arr), &mut out);
    }

    let Some(choice) = data.pointer("/choices/0") else {
        return Ok(out);
    };

    if let Some(delta) = choice.get("delta") {
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(LlmEvent::Text(text.to_string()));
            }
        }
        if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(LlmEvent::Reasoning(text.to_string()));
            }
        }
        if let Some(text) = delta.get("reasoning").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(LlmEvent::Reasoning(text.to_string()));
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                let Some(index) = tc.get("index").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let index = index as u32;
                let entry = tools.entry(index).or_default();
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    entry.id = id.to_string();
                }
                if let Some(func) = tc.get("function") {
                    if let Some(n) = func.get("name").and_then(|v| v.as_str()) {
                        entry.name = n.to_string();
                    }
                    if let Some(a) = func.get("arguments").and_then(|v| v.as_str()) {
                        if !a.is_empty() {
                            entry.arguments.push_str(a);
                            // Surface the raw arg fragment as it streams (#194) so
                            // heads can render file-sized `edit`/`write` inputs
                            // before the assembled `ToolCall` flushes at finish.
                            out.push(LlmEvent::ToolCallDelta {
                                id: entry.id.clone(),
                                name: entry.name.clone(),
                                delta: a.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str());
    tracing::debug!(
        finish_reason,
        has_tools = !tools.is_empty(),
        "openai-compat chunk"
    );

    Ok(out)
}

/// Flush every assembled tool in `tools` as a validated [`LlmEvent::ToolCall`],
/// skipping any whose streamed `arguments` are not a JSON object (a model
/// glitch, or corruption from the UTF-8 chunk-boundary bug, #443) instead of
/// forwarding malformed `input` downstream. Returns whether at least one call
/// was actually emitted, so the caller can degrade `stop_reason` the same way
/// regardless of which path reached the end of the stream (ADR-0118, #445) —
/// this is the single flush site both the explicit `finish_reason ==
/// "tool_calls"` case and the no-finish-reason fallback fall through to.
pub(super) fn flush_pending_tools(
    tools: &mut BTreeMap<u32, PendingTool>,
    out: &mut Vec<LlmEvent>,
) -> bool {
    let mut emitted_any = false;
    for (_, t) in std::mem::take(tools) {
        let should_emit = if t.arguments.is_empty() {
            true
        } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t.arguments) {
            matches!(v, serde_json::Value::Object(_))
        } else {
            tracing::warn!(
                tool = %t.name,
                args = %t.arguments,
                "skipping tool call with malformed JSON arguments"
            );
            false
        };

        if should_emit {
            emitted_any = true;
            out.push(LlmEvent::ToolCall(t.into_tool_call()));
        }
    }
    emitted_any
}

/// Render a z.ai `web_search` source array (#305) as one `[web_search] {title} —
/// {url}` [`LlmEvent::Reasoning`] line per entry, plus one persisted
/// `ContentBlock` (#481) carrying a newline-joined `summary` of those same
/// lines — the z.ai wire has no documented replay format for a search block, so
/// `summary` (rendered as plain text by every converter, #481's
/// `assistant_text`) is the whole persisted representation; there is no opaque
/// `data` payload worth keeping beyond the entries themselves, which ride in
/// `data` unmodified only so a future verified shape (item 3 of #481) has
/// something to refine without another wire change. A non-array (or an entry
/// with neither title nor link) is ignored — this is a best-effort, defensive
/// surface.
fn emit_web_search(value: Option<&Value>, out: &mut Vec<LlmEvent>) {
    let Some(entries) = value.and_then(|v| v.as_array()) else {
        return;
    };
    let mut lines: Vec<String> = Vec::new();
    for entry in entries {
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let url = entry
            .get("link")
            .or_else(|| entry.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if title.is_empty() && url.is_empty() {
            continue;
        }
        lines.push(format!("[web_search] {title} — {url}"));
    }
    for line in &lines {
        out.push(LlmEvent::Reasoning(line.clone()));
    }
    if !lines.is_empty() {
        out.push(LlmEvent::ContentBlock(ContentPart::provider_search(
            "zai",
            lines.join("\n"),
            Value::Array(entries.clone()),
        )));
    }
}
