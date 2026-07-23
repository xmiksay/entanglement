//! Request-body construction: `entanglement`'s `Message` history → the
//! OpenAI Chat Completions wire shape. Split out of `openai/mod.rs` (#481) to
//! keep the streaming client itself under the file-size cap.

use crate::web_search::WebSearchConfig;
use crate::{ContentPart, GenerationParams, ImageSource, Message, MessageRole, ToolSpec};
use serde_json::{json, Value};

pub(super) fn build_body(
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    generation: Option<GenerationParams>,
    web_search: Option<&WebSearchConfig>,
) -> Value {
    let mut msgs = Vec::with_capacity(messages.len() + 1);
    if !system.is_empty() {
        msgs.push(json!({ "role": "system", "content": system }));
    }
    msgs.extend(convert_messages(messages));
    let mut body = json!({
        "model": model,
        "messages": msgs,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    // Function tools (core-advertised) plus the opt-in provider-side `web_search`
    // entry (#305). The z.ai server tool rides the same `tools` array, so it is
    // requestable even when no function tools are present.
    let mut tool_entries = convert_tools(tools);
    if let Some(ws) = web_search {
        tool_entries.push(web_search_tool_entry(ws));
    }
    if !tool_entries.is_empty() {
        body["tools"] = Value::Array(tool_entries);
    }
    // Generation knobs the head resolved for this model (#191). The OpenAI-compat
    // wire carries temperature + `max_tokens`; it has no standard thinking-budget
    // field, so `thinking_budget_tokens` is dropped here (the Anthropic wire owns
    // that channel). `reasoning_effort` (#374) is OpenAI's own native field —
    // passed through verbatim, the one wire that needs no mapping.
    if let Some(g) = generation {
        if let Some(temp) = g.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max) = g.max_output_tokens {
            body["max_tokens"] = json!(max);
        }
        if let Some(effort) = g.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
    }
    body
}

/// Map entanglement's `Message` history to OpenAI chat format. Tool results become one
/// `role: "tool"` message each (with its `tool_call_id`); assistant tool calls
/// become a `tool_calls` array carrying the raw JSON argument string.
pub(super) fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            MessageRole::User => {
                out.push(json!({ "role": "user", "content": openai_content(&m.content) }));
            }
            MessageRole::Assistant => {
                let mut entry =
                    json!({ "role": "assistant", "content": assistant_text(&m.content) });
                if !m.tool_calls.is_empty() {
                    let calls: Vec<Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            let args = if tc.input.is_empty() {
                                "{}".to_string()
                            } else {
                                tc.input.clone()
                            };
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": { "name": tc.name, "arguments": args },
                            })
                        })
                        .collect();
                    entry["tool_calls"] = Value::Array(calls);
                }
                out.push(entry);
            }
            MessageRole::Tool => {
                // OpenAI's `role: "tool"` message only accepts string content, so
                // an image tool result (#221 `read`) can't ride inside it. The
                // text (if any) stays on the tool message; the image blocks are
                // handed to the model as a following `role: "user"` message with
                // `image_url` parts, keeping the `tool_call_id` linkage intact.
                let images: Vec<ContentPart> = m
                    .content
                    .iter()
                    .filter(|p| matches!(p, ContentPart::Image { .. }))
                    .cloned()
                    .collect();
                let text = m.text();
                let content = if text.is_empty() && !images.is_empty() {
                    "[image returned; see the following message]".to_string()
                } else {
                    text
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": content,
                }));
                if !images.is_empty() {
                    out.push(json!({ "role": "user", "content": openai_content(&images) }));
                }
            }
        }
    }
    out
}

/// Render a message's content to OpenAI's `content` field. All-text collapses to
/// a plain string (the common case, smaller wire); any image part switches to the
/// multimodal block array (`text` / `image_url` with a `data:` URL, #197/#221). A
/// [`ContentPart::ProviderSearch`] block (#481) always renders as its `summary`
/// text — the OpenAI-compat wire has no native block format to replay a search
/// call/result verbatim, so `summary` (never the opaque `data`) is the portable
/// fallback, regardless of which provider minted it.
fn openai_content(content: &[ContentPart]) -> Value {
    if content
        .iter()
        .all(|p| matches!(p, ContentPart::Text { .. }))
    {
        return Value::String(crate::content_text(content));
    }
    let blocks: Vec<Value> = content
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => json!({ "type": "text", "text": text }),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            } => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") },
            }),
            ContentPart::ProviderSearch { summary, .. } => {
                json!({ "type": "text", "text": summary })
            }
        })
        .collect();
    Value::Array(blocks)
}

/// An assistant message's `content` string: its text parts, plus any
/// [`ContentPart::ProviderSearch`] block's `summary` appended as its own line
/// (#481) — otherwise a search call/result minted earlier in the conversation
/// (by this provider or, after a live `/model` switch, another one) would
/// silently vanish from the request OpenAI-compat sends, since an assistant
/// message here is a plain string with no block-array form.
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
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema,
                }
            })
        })
        .collect()
}

/// The z.ai provider-side web-search tools entry (#305):
/// `{"type":"web_search","web_search":{...}}`. `search_result: true` asks z.ai to
/// return the source list (so `handle_chunk` can surface citations); `count` and
/// `search_domain_filter` map the optional config knobs when set.
fn web_search_tool_entry(ws: &WebSearchConfig) -> Value {
    let mut inner = json!({
        "enable": true,
        "search_result": true,
    });
    if let Some(max) = ws.max_uses {
        inner["count"] = json!(max);
    }
    if !ws.allowed_domains.is_empty() {
        // z.ai's domain filter is a single string; join the configured domains.
        inner["search_domain_filter"] = json!(ws.allowed_domains.join(","));
    }
    json!({ "type": "web_search", "web_search": inner })
}
