//! SSE chunk → [`LlmEvent`] mapping for the Gemini wire — the pure parsing
//! half of `gemini.rs`, split out along the 400-line file cap (#684 grew the
//! parent with the OAuth-bearer request loop). No I/O anywhere here.

use serde_json::{json, Value};

use crate::{LlmEvent, ToolCall, Usage};

/// Extract the JSON payload from one SSE frame (`data: <json>` lines, joined).
/// Returns `None` for a comment/keep-alive/blank frame or unparsable data.
pub(super) fn parse_frame(frame: &str) -> Option<Value> {
    let mut data_parts: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_parts.push(rest.trim());
        }
    }
    if data_parts.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&data_parts.join("\n")).ok()
}

/// Map one parsed chunk to zero or more [`LlmEvent`]s, folding usage + the latest
/// `finishReason`. Pure (no I/O) so it unit-tests directly. A `functionCall` part
/// is assembled immediately — Gemini sends the whole arg object, not streamed —
/// and its `thoughtSignature` (if any) is stashed into `provider_meta` (#309).
pub(super) fn handle_chunk(
    data: &Value,
    usage: &mut Usage,
    finish_reason: &mut Option<String>,
    tool_call_ordinal: &mut usize,
) -> Result<Vec<LlmEvent>, anyhow::Error> {
    let mut out = Vec::new();

    if let Some(parts) = data
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
    {
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                out.push(LlmEvent::ToolCall(function_call_to_tool_call(
                    fc,
                    part,
                    *tool_call_ordinal,
                )));
                *tool_call_ordinal += 1;
            } else if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if text.is_empty() {
                    continue;
                }
                // A `thought: true` part is the model's extended reasoning.
                if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                    out.push(LlmEvent::Reasoning(text.to_string()));
                } else {
                    out.push(LlmEvent::Text(text.to_string()));
                }
            }
        }
    }

    if let Some(r) = data
        .pointer("/candidates/0/finishReason")
        .and_then(|v| v.as_str())
    {
        *finish_reason = Some(r.to_string());
    }

    if let Some(meta) = data.get("usageMetadata") {
        apply_usage(meta, usage);
    }

    Ok(out)
}

/// Build a [`ToolCall`] from a Gemini `functionCall` part. The id is
/// `name#ordinal` (#444) — unique per stream even when the same tool is
/// called in parallel — while `name` stays bare; [`super::tool_name_from_id`]
/// recovers it when the reply is sent back as a `functionResponse`. The
/// `thoughtSignature` (a thinking model's opaque per-call token) is preserved
/// in `provider_meta` for verbatim round-trip on the next turn (#309).
fn function_call_to_tool_call(fc: &Value, part: &Value, ordinal: usize) -> ToolCall {
    let name = fc
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
    let input = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
    let provider_meta = part
        .get("thoughtSignature")
        .and_then(|v| v.as_str())
        .map(|sig| json!({ super::THOUGHT_SIGNATURE_KEY: sig }));
    ToolCall {
        id: super::synthesize_tool_call_id(&name, ordinal),
        name,
        input,
        provider_meta,
    }
}

/// Fold Gemini's `usageMetadata` into the normalized [`Usage`]. `promptTokenCount`
/// is the whole prompt including any cached read, so subtract the cached portion to
/// keep `input_tokens` uncached (no double-count against catalog pricing, #192).
///
/// Thinking tokens are billed as output but reported *separately* from
/// `candidatesTokenCount`, so `thoughtsTokenCount` is summed into `output_tokens`
/// rather than given a `Usage` field of its own: the four `Usage` dimensions map
/// 1:1 onto the four [`ModelPricing`](crate::ModelPricing) dimensions, and a fifth
/// would have no rate to bill against. Anthropic and OpenAI already fold thinking
/// into their output figure server-side, so this makes Gemini consistent with them
/// instead of silently under-reporting `output_tokens` (and `cost_usd`) for every
/// thinking model.
fn apply_usage(meta: &Value, usage: &mut Usage) {
    let cached = meta.get("cachedContentTokenCount").and_then(|v| v.as_u64());
    if let Some(prompt) = meta.get("promptTokenCount").and_then(|v| v.as_u64()) {
        usage.input_tokens = Some(prompt.saturating_sub(cached.unwrap_or(0)));
    }
    if let Some(c) = cached {
        usage.cached_input_tokens = Some(c);
    }
    // A pure-thinking chunk reports thoughts with no `candidatesTokenCount`; still
    // record it, or those tokens are billed by the provider and counted by nobody.
    let candidates = meta.get("candidatesTokenCount").and_then(|v| v.as_u64());
    let thoughts = meta.get("thoughtsTokenCount").and_then(|v| v.as_u64());
    if candidates.is_some() || thoughts.is_some() {
        usage.output_tokens = Some(
            candidates
                .unwrap_or(0)
                .saturating_add(thoughts.unwrap_or(0)),
        );
    }
}
