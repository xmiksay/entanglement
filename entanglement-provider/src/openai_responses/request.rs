//! Request-body construction: `entanglement`'s `Message` history → the
//! OpenAI Responses API `input` item array. Split out of
//! `openai_responses/mod.rs` (mirrors `openai/request.rs`) to keep the
//! streaming client itself under the file-size cap.
//!
//! The Responses wire is flat and typed: `input` is an array of items, not
//! Chat Completions' role+content messages. A `function_call` correlates to
//! its `function_call_output` by `call_id`; this client's reserved
//! `tool_search_call`/`tool_search_output` pair (ADR-0196 §3) does the same,
//! keyed off [`super::TOOL_SEARCH_CALL_TOOL`] on the assistant side and
//! [`ContentPart::ToolSearchOutput`] on the reply side.

use crate::{
    ContentPart, GenerationParams, ImageSource, Message, MessageRole, ThinkingSpec, ToolSpec,
};
use serde_json::{json, Value};

use super::TOOL_SEARCH_CALL_TOOL;

/// The provider tag [`ContentPart::ToolSearchOutput::provider`] is stamped
/// with and matched on replay — mirrors [`crate::ContentPart::Reasoning`]'s
/// per-wire `provider` tag (ADR-0160's "opaque to anyone but its author"
/// contract, reused here for the `tool_search_output` payload).
const WIRE_NAME: &str = "openai_responses";

pub(super) fn build_body(
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    generation: Option<GenerationParams>,
    thinking: ThinkingSpec,
) -> Value {
    let mut body = json!({
        "model": model,
        "input": convert_messages(messages),
        "stream": true,
    });
    if !system.is_empty() {
        body["instructions"] = json!(system);
    }
    let any_deferred = tools.iter().any(|t| t.defer_loading);
    let mut tool_entries = convert_tools(tools);
    if any_deferred {
        tool_entries.push(tool_search_entry());
    }
    if !tool_entries.is_empty() {
        body["tools"] = Value::Array(tool_entries);
    }
    let requested_effort = generation.as_ref().and_then(|g| g.reasoning_effort);
    if let Some(g) = generation {
        if let Some(temp) = g.temperature {
            body["temperature"] = json!(temp);
        }
        // Responses' field is `max_output_tokens`, not Chat Completions'
        // `max_tokens` — same knob, different wire name.
        if let Some(max) = g.max_output_tokens {
            body["max_output_tokens"] = json!(max);
        }
    }
    // Responses' field is a `reasoning` object, not a flat `reasoning_effort`
    // string — same knob, different wire shape — carrying only a tier the
    // request's model accepts (ADR-0203's per-model clamp).
    if let Some(effort) = thinking.resolve_effort(requested_effort).effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    body
}

/// Map history to Responses `input` items. An assistant tool call named
/// [`TOOL_SEARCH_CALL_TOOL`] becomes a `tool_search_call` item instead of
/// `function_call`; the paired reply carries a
/// [`ContentPart::ToolSearchOutput`] minted by this wire and becomes a
/// `tool_search_output` item instead of `function_call_output`. Every other
/// shape (text, images, a foreign-wire `ToolReference`/`ToolSearchOutput`
/// block) degrades the same way the OpenAI-compat client does.
pub(super) fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            MessageRole::User => {
                out.push(json!({ "role": "user", "content": responses_content(&m.content) }));
            }
            MessageRole::Assistant => {
                let text = assistant_text(&m.content);
                if !text.is_empty() {
                    out.push(json!({ "role": "assistant", "content": text }));
                }
                for tc in &m.tool_calls {
                    if tc.name == TOOL_SEARCH_CALL_TOOL {
                        let arguments: Value =
                            serde_json::from_str(&tc.input).unwrap_or_else(|_| json!({}));
                        out.push(json!({
                            "type": "tool_search_call",
                            "execution": "client",
                            "call_id": tc.id,
                            "status": "completed",
                            "arguments": arguments,
                        }));
                    } else {
                        let arguments = if tc.input.is_empty() {
                            "{}".to_string()
                        } else {
                            tc.input.clone()
                        };
                        out.push(json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.name,
                            "arguments": arguments,
                        }));
                    }
                }
            }
            MessageRole::Tool => {
                let call_id = m.tool_call_id.clone().unwrap_or_default();
                let search_data = m.content.iter().find_map(|p| match p {
                    ContentPart::ToolSearchOutput { provider, data, .. }
                        if provider == WIRE_NAME =>
                    {
                        Some(data.clone())
                    }
                    _ => None,
                });
                if let Some(tools_data) = search_data {
                    out.push(json!({
                        "type": "tool_search_output",
                        "execution": "client",
                        "call_id": call_id,
                        "status": "completed",
                        "tools": tools_data,
                    }));
                } else {
                    let mut text = m.text();
                    for p in &m.content {
                        match p {
                            ContentPart::ToolReference { tool_name } => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(&crate::tool_reference_fallback_text(tool_name));
                            }
                            // A `ToolSearchOutput` minted by a *different*
                            // provider (history replayed after a live
                            // `/model` switch onto this wire) — degrade to
                            // its portable summary rather than drop it.
                            ContentPart::ToolSearchOutput { summary, .. } => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(summary);
                            }
                            _ => {}
                        }
                    }
                    out.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": text,
                    }));
                }
                let images: Vec<ContentPart> = m
                    .content
                    .iter()
                    .filter(|p| matches!(p, ContentPart::Image { .. }))
                    .cloned()
                    .collect();
                if !images.is_empty() {
                    out.push(json!({ "role": "user", "content": responses_content(&images) }));
                }
            }
        }
    }
    out
}

/// Render content to the Responses `content` field: all-text collapses to a
/// plain string (mirrors the OpenAI-compat client's `openai_content`); any
/// image switches to the typed multimodal part array (`input_text`/
/// `input_image`, a `data:` URL).
fn responses_content(content: &[ContentPart]) -> Value {
    if content
        .iter()
        .all(|p| matches!(p, ContentPart::Text { .. }))
    {
        return Value::String(crate::content_text(content));
    }
    let parts: Vec<Value> = content
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => json!({ "type": "input_text", "text": text }),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            } => json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            }),
            ContentPart::ProviderSearch { summary, .. } => {
                json!({ "type": "input_text", "text": summary })
            }
            ContentPart::Reasoning { .. } => json!(null),
            ContentPart::ToolReference { tool_name } => json!({
                "type": "input_text",
                "text": crate::tool_reference_fallback_text(tool_name),
            }),
            ContentPart::ToolSearchOutput { summary, .. } => {
                json!({ "type": "input_text", "text": summary })
            }
        })
        .filter(|v| !v.is_null())
        .collect();
    Value::Array(parts)
}

/// An assistant message's `content` string: its text parts plus any
/// [`ContentPart::ProviderSearch`] summary appended as its own line (mirrors
/// the OpenAI-compat client's `assistant_text`). No thinking-rail handling —
/// this client doesn't (yet) capture/replay a Responses `reasoning` item;
/// see the module doc.
fn assistant_text(content: &[ContentPart]) -> String {
    let mut text = crate::content_text(content);
    for p in content {
        if let ContentPart::ProviderSearch { summary, .. } = p {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(summary);
        }
    }
    text
}

fn convert_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut entry = json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.schema,
            });
            // Sent on every request regardless — the API needs the full
            // definition to run search — mirrors the Anthropic client's
            // `defer_loading` passthrough (ADR-0196 §3).
            if t.defer_loading {
                entry["defer_loading"] = json!(true);
            }
            entry
        })
        .collect()
}

/// The client-executed `tool_search` tools-array entry (ADR-0196 §3,
/// wire reference §2.2/§2.5): declared only when at least one tool above
/// carries `defer_loading` (mirrors Anthropic's "at least one non-deferred
/// tool" requirement in spirit — here it's simply pointless to declare
/// search with nothing to discover). The `query` field is this client's own
/// schema choice (the API only requires *some* schema for client-execution
/// mode) — the runtime's `tool_search` dispatch treats it as a free-text
/// filter, same shape as `explore`'s `filter` argument.
fn tool_search_entry() -> Value {
    json!({
        "type": "tool_search",
        "execution": "client",
        "description": "Search for tools needed to continue the task — \
            matches against each deferred tool's name and description.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": { "type": "string" }
            },
            "required": ["query"],
            "additionalProperties": false,
        }
    })
}
