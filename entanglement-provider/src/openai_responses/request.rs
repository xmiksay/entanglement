//! Request-body construction: `entanglement`'s `Message` history → the
//! OpenAI Responses API `input` item array. Split out of
//! `openai_responses/mod.rs` (mirrors `openai/request.rs`) to keep the
//! streaming client itself under the file-size cap.
//!
//! The Responses wire is flat and typed: `input` is an array of items, not
//! Chat Completions' role+content messages. A `function_call` correlates to
//! its `function_call_output` by `call_id`.

use crate::{
    ContentPart, GenerationParams, ImageSource, Message, MessageRole, ThinkingSpec, ToolSpec,
};
use serde_json::{json, Value};

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
    let tool_entries = convert_tools(tools);
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

/// Map history to Responses `input` items. Every shape (text, images, a
/// persisted `ToolReference`/`ToolSearchOutput` block from a pre-ADR-0226
/// session) degrades the same way the OpenAI-compat client does — neither
/// block is minted any more, but both must keep replaying from old logs.
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
            MessageRole::Tool => {
                let call_id = m.tool_call_id.clone().unwrap_or_default();
                let mut text = m.text();
                for p in &m.content {
                    match p {
                        ContentPart::ToolReference { tool_name } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&crate::tool_reference_fallback_text(tool_name));
                        }
                        // A `ToolSearchOutput` minted by *any* provider (a
                        // pre-ADR-0226 session replaying after a live `/model`
                        // switch onto this wire) — degrade to its portable
                        // summary rather than drop it.
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
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.schema,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentPart, ToolCall};

    fn tool_msg(id: &str, parts: Vec<ContentPart>) -> Message {
        Message::tool_content(id, parts)
    }

    #[test]
    fn a_tool_search_output_block_degrades_to_its_summary() {
        // WIRE_NAME-tagged blocks were the pre-ADR-0226 native delivery; they
        // still replay from persisted history as plain text.
        let msgs = [tool_msg(
            "c1",
            vec![ContentPart::ToolSearchOutput {
                provider: "openai_responses".into(),
                summary: "loaded get_weather".into(),
                data: serde_json::json!([{"name": "get_weather"}]),
            }],
        )];
        let items = convert_messages(&msgs);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["output"], "loaded get_weather");
    }

    #[test]
    fn every_call_is_a_plain_function_call() {
        let m = Message::assistant(
            "thinking",
            vec![ToolCall::new("c1", "mcp__gh__search", r#"{"q":"x"}"#)],
        );
        let items = convert_messages(std::slice::from_ref(&m));
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["name"], "mcp__gh__search");
    }

    #[test]
    fn tools_carry_no_defer_loading() {
        let specs = [ToolSpec::new("read", "read a file")];
        let entries = convert_tools(&specs);
        assert!(entries[0].get("defer_loading").is_none());
    }
}
