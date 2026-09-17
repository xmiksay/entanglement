//! Request-body construction: `entanglement`'s `Message` history → the
//! Anthropic Messages API wire shape. Split out of `anthropic/mod.rs` (#481)
//! to keep the streaming client itself under the file-size cap.

use crate::web_search::WebSearchConfig;
use crate::{
    AnthropicModelSpec, ContentPart, GenerationParams, ImageSource, Message, MessageRole,
    ThinkingStyle, ToolSpec,
};
use serde_json::{json, Value};

// The two extended-thinking request shapes, split out for the 400-line cap.
mod thinking;
use thinking::{apply_adaptive_thinking, apply_budget_thinking};

/// Fallback Anthropic web-search server-tool type when no `ModelEntry`
/// capability flag names a newer one (#481, follow-up to #305/ADR-0075's
/// hardcoded `_20250305`).
const DEFAULT_WEB_SEARCH_TOOL_VERSION: &str = "web_search_20250305";
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
    spec: AnthropicModelSpec,
    trailing_notice: Option<&str>,
) -> Value {
    let g = generation.unwrap_or_default();
    let mut max_tokens = g.max_output_tokens.unwrap_or(default_max_tokens);
    let mut messages = convert_messages(messages, spec.replay_thinking);
    place_history_breakpoint(&mut messages);
    // Appended *after* the breakpoint so the notice is invisible to anchor
    // placement (it is never in the ~20-block lookback window a cache write
    // needs to match) and carries no `cache_control` of its own — see
    // `append_final_user_block`'s doc for why this fixes the prompt-cache
    // bug `LlmRequest::trailing_notice` exists to avoid.
    if let Some(notice) = trailing_notice {
        append_final_user_block(
            &mut messages,
            json!({ "type": "text", "text": notice }),
            "content",
        );
    }
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        // Standard breakpoint placement (#566, ADR-0202): end of tools, end of
        // system, last user turn (plus a deeper history anchor, #673).
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
    // Must land on a non-deferred entry — Anthropic 400s a `defer_loading`
    // tool carrying `cache_control` (ADR-0196 §3) — so scan back from the end
    // rather than assume the last entry qualifies (the alphabetically-last
    // tool can easily be deferred under `anthropic_native`).
    if let Some(last_cacheable) = tool_entries
        .iter_mut()
        .rev()
        .find(|t| t.get("defer_loading").and_then(Value::as_bool) != Some(true))
    {
        last_cacheable["cache_control"] = json!({ "type": "ephemeral" });
    }
    if !tool_entries.is_empty() {
        body["tools"] = Value::Array(tool_entries);
    }
    // Extended thinking (#191). Anthropic has two mutually exclusive request
    // shapes and the catalog says which one this model takes — the newer models
    // reject `budget_tokens` with a 400, so the choice cannot be a client
    // constant. With thinking on (either shape), `temperature` may only be its
    // default, so it is omitted; with thinking off it passes through — unless
    // the model rejects sampling parameters outright (ADR-0203), where a
    // temperature set live by `SetGeneration` would 400 every request.
    let thinking_on = match spec.thinking_style {
        ThinkingStyle::Budget => apply_budget_thinking(&mut body, &g, &mut max_tokens),
        ThinkingStyle::Adaptive => apply_adaptive_thinking(&mut body, &g, spec.effort_tiers),
    };
    if !thinking_on && spec.supports_temperature {
        if let Some(temp) = g.temperature {
            body["temperature"] = json!(temp);
        }
    }
    body
}

/// Map entanglement's `Message` history to Anthropic's content-block format. Runs of
/// consecutive tool-result messages are merged into a single `user` turn
/// (Anthropic requires all `tool_result` blocks for a turn in one message).
///
/// `replay_reasoning` enables replaying captured thinking blocks on **every**
/// assistant message (ADR-0202). Preserved-thinking models (Opus 4.5+, Sonnet
/// 4.6+, Fable) keep prior-turn thinking server-side, so stripping it edits
/// history at each earlier assistant position — a prompt-cache bust every
/// round once the near breakpoint covers the last assistant turn, and a 400 on
/// Fable 5.1 for new accounts. Older models ignore prior-turn blocks unbilled,
/// so replaying everywhere is safe there too.
fn convert_messages(messages: &[Message], replay_reasoning: bool) -> Vec<Value> {
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
                let mut blocks: Vec<Value> =
                    anthropic_blocks(&messages[i].content, replay_reasoning);
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

/// Mark the history breakpoints (#566, #673, ADR-0202): the last content block
/// of the **last** `user`-role message (near), plus a deeper anchor on the
/// third-to-last.
///
/// The near anchor sits on the newest turn, so everything this request sends
/// is written to the cache now and read back next round. Anchoring one turn
/// earlier (the pre-ADR-0202 placement, meant to spare a steered/edited retry
/// a wasted write) billed that tail uncached this round *and* as a cache write
/// next round — ~2.25× versus 1.25× — while the retry it protected costs at
/// most one wasted write.
///
/// The deeper anchor (#673) exists because Anthropic's cache lookup only
/// scans ~20 content blocks upstream of each explicit breakpoint: the near
/// anchor advances every round, and one round can append several user-role
/// messages (the prompt plus one merged tool-result turn per batch), so a
/// large parallel tool batch alone can push the previous round's cached
/// entry out of the lookback window — re-writing the whole history span from
/// the tools/system prefix at the cache-write rate. A second marker further
/// back guarantees a match point that survives the near anchor's
/// neighborhood changing.
///
/// The two anchors are distinct by construction, so with system (1) + tools
/// (1) + history (≤2) the request carries at most 4 markers, exactly the API
/// cap (a 5th is a 400, locked in by test).
fn place_history_breakpoint(messages: &mut [Value]) {
    let user_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(i, _)| i)
        .collect();
    let near = user_idxs.last();
    let deep = user_idxs.iter().rev().nth(2);
    for &idx in near.into_iter().chain(deep) {
        if let Some(last_block) = messages[idx]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|blocks| blocks.last_mut())
        {
            last_block["cache_control"] = json!({ "type": "ephemeral" });
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

/// Append `block` as the wire's final user-role turn (the trailing-notice
/// fix, see `LlmRequest::trailing_notice`'s doc): merged into an existing
/// trailing user turn when there is one, rather than pushed as a new
/// message, because Anthropic and Gemini both reject non-alternating roles
/// (`coalesce_same_role`'s doc above) and a request built after tool results
/// or a plain user prompt already ends in a `user` turn. Deliberately no
/// `cache_control` on `block` and no re-run of anchor placement — callers
/// invoke this *after* `place_history_breakpoint`, so the notice never
/// occupies a marker and never sits in the position the near anchor
/// re-checks next round.
///
/// `pub(crate)` — reused by `crate::gemini::request`, which faces the
/// identical alternating-role constraint (`coalesce_same_role`'s own doc).
pub(crate) fn append_final_user_block(messages: &mut Vec<Value>, block: Value, content_key: &str) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(arr) = last.get_mut(content_key).and_then(Value::as_array_mut) {
                arr.push(block);
                return;
            }
        }
    }
    let mut obj = serde_json::Map::new();
    obj.insert("role".to_string(), json!("user"));
    obj.insert(content_key.to_string(), json!([block]));
    messages.push(Value::Object(obj));
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
/// here rather than constraining core. [`ContentPart::ToolReference`]
/// (ADR-0196 §3) is handled inline below.
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
            // ADR-0196 §3: the only wire with a native `tool_reference` — no
            // "foreign provider" case to gate on, unlike the two above.
            ContentPart::ToolReference { tool_name } => rest.push(json!({
                "type": "tool_reference",
                "tool_name": tool_name,
            })),
            // ADR-0196 §3: a `tool_search_output` block persisted from a
            // `responses_native` session (e.g. history replaying after a
            // live `/model` switch to this wire) has no native mechanism
            // here — the Anthropic wire has its own `tool_reference`
            // primitive instead — so degrade to `summary` text rather than
            // silently dropping the "discovered X" outcome, mirroring
            // `ProviderSearch`'s foreign-provider fallback.
            ContentPart::ToolSearchOutput { summary, .. } => {
                rest.push(json!({ "type": "text", "text": summary }))
            }
        }
    }
    reasoning.extend(rest);
    reasoning
}

fn convert_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut entry = json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.schema,
            });
            // ADR-0196 §3: sent on every request regardless (the API needs
            // the full definition to run search + expand references) —
            // `defer_loading` only controls prompt/cache-key rendering.
            if t.defer_loading {
                entry["defer_loading"] = json!(true);
            }
            entry
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
