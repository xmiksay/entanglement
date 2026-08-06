//! Request-body construction: `entanglement`'s `Message` history → the
//! Anthropic Messages API wire shape. Split out of `anthropic/mod.rs` (#481)
//! to keep the streaming client itself under the file-size cap.

use crate::web_search::WebSearchConfig;
use crate::{
    ContentPart, GenerationParams, ImageSource, Message, MessageRole, ReasoningEffort,
    ThinkingStyle, ToolSpec,
};
use serde_json::{json, Value};

/// Fallback Anthropic web-search server-tool type when no `ModelEntry`
/// capability flag names a newer one (#481, follow-up to #305/ADR-0075's
/// hardcoded `_20250305`).
const DEFAULT_WEB_SEARCH_TOOL_VERSION: &str = "web_search_20250305";
/// Thinking-budget tokens for [`ReasoningEffort::High`] when the request sets no
/// explicit [`GenerationParams::thinking_budget_tokens`] (#374) — Anthropic has
/// no effort concept of its own, so `reasoning_effort` maps onto a thinking
/// tier here instead.
const HIGH_EFFORT_THINKING_BUDGET: u32 = 32_000;
/// Thinking-budget tokens for [`ReasoningEffort::Medium`] (#374).
const MEDIUM_EFFORT_THINKING_BUDGET: u32 = 8_000;
/// Bump amount for `max_tokens` when a thinking budget would otherwise swallow
/// the whole cap (mirrors the client's own [`super::DEFAULT_MAX_TOKENS`]
/// fallback so this module stays self-contained).
const MAX_TOKENS_BUDGET_HEADROOM: u32 = 16_384;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_body(
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    default_max_tokens: u32,
    generation: Option<GenerationParams>,
    web_search: Option<&WebSearchConfig>,
    web_search_tool_version: Option<&str>,
    thinking_style: ThinkingStyle,
    replay_thinking: bool,
) -> Value {
    let g = generation.unwrap_or_default();
    let mut max_tokens = g.max_output_tokens.unwrap_or(default_max_tokens);
    let mut messages = convert_messages(messages, replay_thinking);
    place_history_breakpoint(&mut messages);
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        // Standard breakpoint placement (#566): end of tools, end of system,
        // second-to-last user turn (plus a deeper history anchor, #673).
        // Anthropic's fixed render order is
        // tools → system → messages, and without a `cache_control` anywhere the
        // whole request re-bills at the full input rate every round — the system
        // block plus every tool schema (~10 KB) and the entire growing history.
        // Deliberately not size-gated: a below-minimum prefix (an aux one-shot's
        // tiny system string) is documented as processed normally with the
        // marker inert — no error, no surcharge — so a gate would only buy a
        // token estimator to maintain.
        "system": [{
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": messages,
        "stream": true,
    });
    // Function tools (core-advertised) plus the opt-in provider-side web-search
    // server tool (#305). The server tool rides the same `tools` array, so it is
    // requestable even with no function tools present.
    let mut tool_entries = convert_tools(tools);
    if let Some(ws) = web_search {
        tool_entries.push(web_search_tool_entry(ws, web_search_tool_version));
    }
    if let Some(last) = tool_entries.last_mut() {
        last["cache_control"] = json!({ "type": "ephemeral" });
    }
    if !tool_entries.is_empty() {
        body["tools"] = Value::Array(tool_entries);
    }
    // Extended thinking (#191). Anthropic has two mutually exclusive request
    // shapes and the catalog says which one this model takes — the newer models
    // reject `budget_tokens` with a 400, so the choice cannot be a client
    // constant. With thinking on (either shape), `temperature` may only be its
    // default, so it is omitted; with thinking off it passes through unchanged.
    let thinking_on = match thinking_style {
        ThinkingStyle::Budget => apply_budget_thinking(&mut body, &g, &mut max_tokens),
        ThinkingStyle::Adaptive => apply_adaptive_thinking(&mut body, &g),
    };
    if !thinking_on {
        if let Some(temp) = g.temperature {
            body["temperature"] = json!(temp);
        }
    }
    body
}

/// The fixed-budget shape: `thinking: {type: "enabled", budget_tokens: N}`.
/// An explicit [`GenerationParams::thinking_budget_tokens`] always wins; absent
/// one, `reasoning_effort` (#374 — Anthropic has no effort concept of its own on
/// this shape) derives a tier default, with `Low`/unset leaving thinking off.
/// Anthropic requires `budget_tokens < max_tokens`, so the cap is bumped when the
/// budget would swallow it. Returns whether thinking was enabled.
fn apply_budget_thinking(body: &mut Value, g: &GenerationParams, max_tokens: &mut u32) -> bool {
    let budget = g.thinking_budget_tokens.or(match g.reasoning_effort {
        Some(ReasoningEffort::High) => Some(HIGH_EFFORT_THINKING_BUDGET),
        Some(ReasoningEffort::Medium) => Some(MEDIUM_EFFORT_THINKING_BUDGET),
        Some(ReasoningEffort::Low) | None => None,
    });
    let Some(budget) = budget else {
        return false;
    };
    if budget >= *max_tokens {
        *max_tokens = budget.saturating_add(MAX_TOKENS_BUDGET_HEADROOM);
        body["max_tokens"] = json!(*max_tokens);
    }
    body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    true
}

/// The adaptive shape: `thinking: {type: "adaptive"}` plus `output_config.effort`,
/// where the model decides how much to think. `budget_tokens` is rejected on this
/// shape, so an explicit [`GenerationParams::thinking_budget_tokens`] is *not* an
/// enable signal here — only `reasoning_effort` is, which is the knob that
/// actually survives onto the wire. There is no `max_tokens` headroom bump: with
/// no budget to swallow the cap, the existing value stands. Returns whether
/// thinking was enabled.
fn apply_adaptive_thinking(body: &mut Value, g: &GenerationParams) -> bool {
    let Some(effort) = g.reasoning_effort else {
        return false;
    };
    let effort = match effort {
        ReasoningEffort::High => "high",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::Low => "low",
    };
    body["thinking"] = json!({ "type": "adaptive" });
    body["output_config"] = json!({ "effort": effort });
    true
}

/// Map entanglement's `Message` history to Anthropic's content-block format. Runs of
/// consecutive tool-result messages are merged into a single `user` turn
/// (Anthropic requires all `tool_result` blocks for a turn in one message).
///
/// `replay_reasoning` enables replaying captured thinking blocks, and applies to
/// the **last** assistant message only. That is exactly where Anthropic requires
/// one — the turn whose tool results are coming back — and it is where the
/// provider looks: earlier turns' thinking is stripped server-side, so resending
/// it would spend input tokens on blocks that are discarded on arrival. History
/// still keeps every block, so replay fidelity is unaffected.
fn convert_messages(messages: &[Message], replay_reasoning: bool) -> Vec<Value> {
    let last_assistant = messages
        .iter()
        .rposition(|m| m.role == MessageRole::Assistant);
    let mut out = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match messages[i].role {
            MessageRole::User => {
                if !messages[i].content.is_empty() {
                    let content = anthropic_blocks(&messages[i].content, false);
                    out.push(json!({ "role": "user", "content": content }));
                }
                i += 1;
            }
            MessageRole::Assistant => {
                let replay = replay_reasoning && last_assistant == Some(i);
                let mut blocks: Vec<Value> = anthropic_blocks(&messages[i].content, replay);
                for tc in &messages[i].tool_calls {
                    let input: Value =
                        serde_json::from_str(&tc.input).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": input,
                    }));
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
                i += 1;
            }
            MessageRole::Tool => {
                let mut results: Vec<Value> = Vec::new();
                while i < messages.len() && messages[i].role == MessageRole::Tool {
                    let id = messages[i].tool_call_id.clone().unwrap_or_default();
                    // Anthropic's `tool_result` content is a string for the
                    // text-only case (back-compat) or an array of blocks when the
                    // result carries an image (#221 `read`).
                    let content = if messages[i]
                        .content
                        .iter()
                        .all(|p| matches!(p, ContentPart::Text { .. }))
                    {
                        json!(messages[i].text())
                    } else {
                        json!(anthropic_blocks(&messages[i].content, false))
                    };
                    results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": content,
                    }));
                    i += 1;
                }
                if !results.is_empty() {
                    out.push(json!({ "role": "user", "content": results }));
                }
            }
        }
    }
    coalesce_same_role(out, "content")
}

/// Mark the history breakpoints (#566, #673): the last content block of the
/// second-to-last `user`-role message, plus a second, deeper anchor on the
/// fourth-to-last. The final user turn is the one most likely to still change
/// (a steered/edited retry), so anchoring one turn earlier gives every prior
/// round — the bulk of a growing conversation — a stable, cacheable prefix
/// without re-marking it on every request.
///
/// The deeper anchor (#673) exists because Anthropic's cache lookup only
/// scans ~20 content blocks upstream of each explicit breakpoint: the near
/// anchor advances every round, and one round can append several user-role
/// messages (the prompt plus one merged tool-result turn per batch), so a
/// large parallel tool batch alone can push the previous round's cached
/// entry out of the lookback window — re-writing the whole history span from
/// the tools/system prefix at the cache-write rate. A second marker two user
/// turns further back guarantees a match point that survives the near
/// anchor's neighborhood changing. `nth(3)` rather than `nth(2)` because
/// adjacent user indexes are often the same round (tool-result turns), which
/// would put both anchors inside one round's churn.
///
/// Falls back to the single user message present when there's only one, and
/// never marks the same block twice — with system (1) + tools (1) + history
/// (≤2) the request carries at most 4 markers, exactly the API cap (a 5th is
/// a 400, locked in by test).
fn place_history_breakpoint(messages: &mut [Value]) {
    let user_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(i, _)| i)
        .collect();
    let near = user_idxs.iter().rev().nth(1).or_else(|| user_idxs.last());
    let deep = user_idxs.iter().rev().nth(3);
    let mut marked: Vec<usize> = Vec::with_capacity(2);
    for &idx in near.into_iter().chain(deep) {
        if marked.contains(&idx) {
            continue;
        }
        if let Some(last_block) = messages[idx]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|blocks| blocks.last_mut())
        {
            last_block["cache_control"] = json!({ "type": "ephemeral" });
            marked.push(idx);
        }
    }
}

/// Merge adjacent messages that share a `role` by concatenating their content
/// arrays under `content_key`. Anthropic (and Gemini) reject non-alternating
/// roles, and an ambiguous-stop retry (ADR-0118) can legitimately leave two
/// adjacent user turns — the original prompt and the injected nudge — once an
/// empty assistant round is dropped. Coalescing them into one message keeps the
/// request well-formed without the caller having to reason about turn shape.
///
/// `pub(crate)` — reused by `crate::gemini::request`, which faces the identical
/// non-alternating-role constraint.
pub(crate) fn coalesce_same_role(messages: Vec<Value>, content_key: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        match out.last_mut() {
            Some(prev) if prev.get("role") == msg.get("role") => {
                if let (Some(prev_content), Some(new_content)) = (
                    prev.get_mut(content_key).and_then(Value::as_array_mut),
                    msg.get(content_key).and_then(Value::as_array),
                ) {
                    prev_content.extend(new_content.iter().cloned());
                    continue;
                }
                out.push(msg);
            }
            _ => out.push(msg),
        }
    }
    out
}

/// Render a message's content parts to Anthropic content blocks: `text` /
/// `image` with a base64 source (#197/#221), and a [`ContentPart::ProviderSearch`]
/// block (#481) minted by *this* provider replays verbatim as its raw stored
/// block — one minted by a different provider (a message that crossed a live
/// `/model` switch) is opaque here and dropped, matching the "replays only to
/// the provider that minted it" contract (mirrors `ToolCall.provider_meta`).
///
/// [`ContentPart::Reasoning`] follows the same provider-match rule but is
/// additionally gated on `replay_reasoning`, and is emitted **first**: Anthropic
/// requires the thinking block to lead the assistant message. The turn loop
/// appends content blocks after the round's text, so the ordering is restored
/// here rather than constraining core.
fn anthropic_blocks(content: &[ContentPart], replay_reasoning: bool) -> Vec<Value> {
    let mut reasoning = Vec::new();
    let mut rest = Vec::new();
    for p in content {
        match p {
            ContentPart::Text { text } => rest.push(json!({ "type": "text", "text": text })),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            } => rest.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            })),
            ContentPart::ProviderSearch { provider, data, .. } if provider == "anthropic" => {
                rest.push(data.clone())
            }
            ContentPart::ProviderSearch { .. } => {}
            ContentPart::Reasoning { provider, data, .. }
                if replay_reasoning && provider == "anthropic" =>
            {
                reasoning.push(data.clone())
            }
            // Replay disabled for this model, or a block minted by another
            // provider: an opaque signature is meaningless to anyone but its
            // author, so drop it rather than degrade it to text.
            ContentPart::Reasoning { .. } => {}
        }
    }
    reasoning.extend(rest);
    reasoning
}

fn convert_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.schema,
            })
        })
        .collect()
}

/// The Anthropic provider-side web-search server tool (#305):
/// `{"type":"<version>","name":"web_search"}` plus the optional `max_uses` /
/// `allowed_domains` knobs. `tool_version` is the catalog's per-model
/// `ModelEntry::web_search_tool_version` capability flag (#481, follow-up to
/// the hardcoded `_20250305`); `None` falls back to
/// [`DEFAULT_WEB_SEARCH_TOOL_VERSION`].
fn web_search_tool_entry(ws: &WebSearchConfig, tool_version: Option<&str>) -> Value {
    let mut entry = json!({
        "type": tool_version.unwrap_or(DEFAULT_WEB_SEARCH_TOOL_VERSION),
        "name": "web_search",
    });
    if let Some(max) = ws.max_uses {
        entry["max_uses"] = json!(max);
    }
    if !ws.allowed_domains.is_empty() {
        entry["allowed_domains"] = json!(ws.allowed_domains);
    }
    entry
}

#[cfg(test)]
mod tests;
