//! Request-body construction: `entanglement`'s `Message` history → the
//! Anthropic Messages API wire shape. Split out of `anthropic/mod.rs` (#481)
//! to keep the streaming client itself under the file-size cap.

use crate::web_search::WebSearchConfig;
use crate::{
    ContentPart, GenerationParams, ImageSource, Message, MessageRole, ReasoningEffort, ToolSpec,
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
) -> Value {
    let g = generation.unwrap_or_default();
    let mut max_tokens = g.max_output_tokens.unwrap_or(default_max_tokens);
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": convert_messages(messages),
        "stream": true,
    });
    // Function tools (core-advertised) plus the opt-in provider-side web-search
    // server tool (#305). The server tool rides the same `tools` array, so it is
    // requestable even with no function tools present.
    let mut tool_entries = convert_tools(tools);
    if let Some(ws) = web_search {
        tool_entries.push(web_search_tool_entry(ws, web_search_tool_version));
    }
    if !tool_entries.is_empty() {
        body["tools"] = Value::Array(tool_entries);
    }
    // Extended thinking (#191): enable it with the resolved budget when the head
    // set one. Anthropic requires `budget_tokens < max_tokens`, so bump the cap if
    // the budget would swallow it; and with thinking on, `temperature` may only be
    // its default, so it is omitted. Without a budget, temperature passes through.
    // An explicit `thinking_budget_tokens` always wins; absent one, `reasoning_effort`
    // (#374 — Anthropic has no effort concept of its own) derives a tier default:
    // `High`/`Medium` enable thinking at a fixed budget, `Low`/unset leave it off.
    let budget = g.thinking_budget_tokens.or(match g.reasoning_effort {
        Some(ReasoningEffort::High) => Some(HIGH_EFFORT_THINKING_BUDGET),
        Some(ReasoningEffort::Medium) => Some(MEDIUM_EFFORT_THINKING_BUDGET),
        Some(ReasoningEffort::Low) | None => None,
    });
    if let Some(budget) = budget {
        if budget >= max_tokens {
            max_tokens = budget.saturating_add(MAX_TOKENS_BUDGET_HEADROOM);
            body["max_tokens"] = json!(max_tokens);
        }
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    } else if let Some(temp) = g.temperature {
        body["temperature"] = json!(temp);
    }
    body
}

/// Map entanglement's `Message` history to Anthropic's content-block format. Runs of
/// consecutive tool-result messages are merged into a single `user` turn
/// (Anthropic requires all `tool_result` blocks for a turn in one message).
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match messages[i].role {
            MessageRole::User => {
                if !messages[i].content.is_empty() {
                    let content = anthropic_blocks(&messages[i].content);
                    out.push(json!({ "role": "user", "content": content }));
                }
                i += 1;
            }
            MessageRole::Assistant => {
                let mut blocks: Vec<Value> = anthropic_blocks(&messages[i].content);
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
                        json!(anthropic_blocks(&messages[i].content))
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
fn anthropic_blocks(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(json!({ "type": "text", "text": text })),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            } => Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            })),
            ContentPart::ProviderSearch { provider, data, .. } if provider == "anthropic" => {
                Some(data.clone())
            }
            ContentPart::ProviderSearch { .. } => None,
        })
        .collect()
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
