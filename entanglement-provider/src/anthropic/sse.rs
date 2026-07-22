//! Server-Sent-Events frame parsing: one Anthropic SSE frame → zero or more
//! [`LlmEvent`]s, plus the raw content-block accumulation `mod.rs`'s
//! `pause_turn` continuation loop needs (#481). Split out of `anthropic/mod.rs`
//! to keep the streaming client itself under the file-size cap.
//!
//! # Wire shape
//! - `message_start`            → input token usage
//! - `content_block_start`      → start a `text`/`tool_use`/`server_tool_use`
//!   block, or a complete `web_search_tool_result` block
//! - `content_block_delta`      → `text_delta` / `input_json_delta` /
//!   `thinking_delta`
//! - `content_block_stop`       → finalize the pending block
//! - `message_delta`            → output token usage + `stop_reason`
//! - `message_stop`             → (handled by the caller via stream end)
//! - `error`                    → mid-stream failure

use crate::{ContentPart, LlmEvent, StopReason, Usage};
use serde_json::{json, Value};

pub(super) struct PendingTool {
    pub id: String,
    pub name: String,
    pub input_buf: String,
    /// `true` for a `server_tool_use` block (provider-side web search, #305):
    /// the provider runs it, so on stop it surfaces as `Reasoning` +
    /// `ContentBlock`, **never** a `ToolCall`, and its arg fragments are not
    /// streamed as `ToolCallDelta`.
    pub is_server: bool,
}

/// Split one SSE frame into its `event:` type and parsed `data:` JSON payload.
pub(super) fn parse_frame(frame: &str) -> (String, Option<Value>) {
    let mut event = String::new();
    let mut data_parts: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_parts.push(rest.trim());
        }
    }
    let data = if data_parts.is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(&data_parts.join("\n")).ok()
    };
    (event, data)
}

/// Map one SSE frame to zero or more [`LlmEvent`]s and update assemble/usage
/// state. Tool input is assembled across `content_block_delta` events and
/// finalized on `content_block_stop`. Pure (no I/O) so it unit-tests directly.
///
/// `assembled_blocks` accumulates every finalized content block (text,
/// `tool_use`, `server_tool_use`, `web_search_tool_result`) in its raw
/// Anthropic wire shape, in arrival order — not for [`LlmEvent`] emission, but
/// for `mod.rs`'s `pause_turn` continuation (#481): resending the paused turn's
/// content verbatim is exactly this array. `pause_turn` is set when a
/// `message_delta`'s `stop_reason` is `"pause_turn"`; the caller owns deciding
/// whether/how to continue. Extended-thinking blocks are intentionally not
/// captured here (the signature needed to replay one isn't tracked either,
/// a pre-existing gap this change doesn't widen) — a `pause_turn` that lands
/// mid-thinking-block loses that block on continuation, a narrow accepted
/// limitation.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_frame(
    event: &str,
    data: Option<Value>,
    current_tool: &mut Option<PendingTool>,
    current_text: &mut Option<String>,
    assembled_blocks: &mut Vec<Value>,
    usage: &mut Usage,
    stop_reason: &mut Option<StopReason>,
    pause_turn: &mut bool,
) -> Result<Vec<LlmEvent>, anyhow::Error> {
    let mut out = Vec::new();
    let data = data.unwrap_or(Value::Null);
    match event {
        "message_start" => {
            // Anthropic reports the regular input, cache reads, and cache
            // creation as separate counts, so no split is needed (unlike OpenAI).
            if let Some(t) = data
                .pointer("/message/usage/input_tokens")
                .and_then(|v| v.as_u64())
            {
                usage.input_tokens = Some(t);
            }
            if let Some(t) = data
                .pointer("/message/usage/cache_read_input_tokens")
                .and_then(|v| v.as_u64())
            {
                usage.cached_input_tokens = Some(t);
            }
            if let Some(t) = data
                .pointer("/message/usage/cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
            {
                usage.cache_write_tokens = Some(t);
            }
        }
        "message_delta" => {
            if let Some(t) = data
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
            {
                usage.output_tokens = Some(t);
            }
            if let Some(r) = data.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                *stop_reason = Some(StopReason::from_anthropic(r));
                *pause_turn = r == "pause_turn";
            }
        }
        "content_block_start" => {
            let block_type = data.pointer("/content_block/type").and_then(|v| v.as_str());
            match block_type {
                Some("text") => {
                    *current_text = Some(String::new());
                }
                // A client tool-call block (assembled, then flushed as `ToolCall`)
                // or a provider-side `server_tool_use` block (#305): the latter is
                // executed by the provider, so on stop it surfaces as `Reasoning`
                // + `ContentBlock`, never a `ToolCall`.
                Some(kind @ ("tool_use" | "server_tool_use")) => {
                    let id = data
                        .pointer("/content_block/id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = data
                        .pointer("/content_block/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    *current_tool = Some(PendingTool {
                        id,
                        name,
                        input_buf: String::new(),
                        is_server: kind == "server_tool_use",
                    });
                }
                // The provider streams the executed search's sources (or an error)
                // as a complete `web_search_tool_result` block (#305) — no deltas,
                // so it's captured and finalized right here.
                Some("web_search_tool_result") => {
                    if let Some(block) = data.pointer("/content_block") {
                        assembled_blocks.push(block.clone());
                        emit_web_search_result(block, &mut out);
                    }
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            if let Some(delta) = data.get("delta") {
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            out.push(LlmEvent::Text(text.to_string()));
                            if let Some(t) = current_text.as_mut() {
                                t.push_str(text);
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(tool), Some(partial)) = (
                            current_tool.as_mut(),
                            delta.get("partial_json").and_then(|t| t.as_str()),
                        ) {
                            if !partial.is_empty() {
                                tool.input_buf.push_str(partial);
                                // Surface the raw arg fragment as it streams (#194)
                                // so heads can render file-sized `edit`/`write`
                                // inputs before `content_block_stop` finalizes the
                                // assembled `ToolCall`. A `server_tool_use` block is
                                // provider-internal (#305) — accumulate its query but
                                // don't stream it as a client tool delta.
                                if !tool.is_server {
                                    out.push(LlmEvent::ToolCallDelta {
                                        id: tool.id.clone(),
                                        name: tool.name.clone(),
                                        delta: partial.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                            out.push(LlmEvent::Reasoning(thinking.to_string()));
                        }
                    }
                    Some("signature_delta") => {
                        // Integrity signature, not content; ignore.
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            if let Some(tool) = current_tool.take() {
                let input = if tool.input_buf.is_empty() {
                    "{}".to_string()
                } else {
                    tool.input_buf
                };
                let parsed_input: Value =
                    serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
                if tool.is_server {
                    let block = json!({
                        "type": "server_tool_use",
                        "id": tool.id,
                        "name": tool.name,
                        "input": parsed_input,
                    });
                    assembled_blocks.push(block.clone());
                    // Provider-side web search (#305): surface the query as a
                    // Reasoning line + a persisted ContentBlock (#481); the
                    // provider runs the search itself, so this must never
                    // become a `ToolCall`.
                    let query = parsed_input
                        .get("query")
                        .and_then(|q| q.as_str())
                        .unwrap_or_default();
                    let summary = format!("[web_search] {query}");
                    out.push(LlmEvent::Reasoning(summary.clone()));
                    out.push(LlmEvent::ContentBlock(ContentPart::provider_search(
                        "anthropic",
                        summary,
                        block,
                    )));
                } else {
                    assembled_blocks.push(json!({
                        "type": "tool_use",
                        "id": tool.id.clone(),
                        "name": tool.name.clone(),
                        "input": parsed_input,
                    }));
                    out.push(LlmEvent::ToolCall(crate::ToolCall {
                        id: tool.id,
                        name: tool.name,
                        input,
                        provider_meta: None,
                    }));
                }
            } else if let Some(text) = current_text.take() {
                if !text.is_empty() {
                    assembled_blocks.push(json!({ "type": "text", "text": text }));
                }
            }
        }
        "error" => {
            let msg = data
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("anthropic stream error")
                .to_string();
            return Err(anyhow::anyhow!(msg));
        }
        _ => {}
    }
    Ok(out)
}

/// Render a `web_search_tool_result` block (#305) as `Reasoning` lines — an
/// array of `web_search_result` entries → one `[web_search] {title} — {url}`
/// line each, an error object → a single `[web_search] error: {code}` line —
/// plus one persisted `ContentBlock` (#481) carrying the whole raw `block` and
/// a newline-joined `summary` of those same lines, so a non-Anthropic renderer
/// (or a converter on another provider's wire) has a readable fallback without
/// ever needing `block` itself.
fn emit_web_search_result(block: &Value, out: &mut Vec<LlmEvent>) {
    let content = block.get("content");
    let mut lines: Vec<String> = Vec::new();
    if let Some(entries) = content.and_then(Value::as_array) {
        for entry in entries {
            let title = entry
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let url = entry
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if title.is_empty() && url.is_empty() {
                continue;
            }
            lines.push(format!("[web_search] {title} — {url}"));
        }
    } else if content.and_then(|c| c.get("type")).and_then(|v| v.as_str())
        == Some("web_search_tool_result_error")
    {
        let code = content
            .and_then(|c| c.get("error_code"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        lines.push(format!("[web_search] error: {code}"));
    }
    for line in &lines {
        out.push(LlmEvent::Reasoning(line.clone()));
    }
    if !lines.is_empty() {
        out.push(LlmEvent::ContentBlock(ContentPart::provider_search(
            "anthropic",
            lines.join("\n"),
            block.clone(),
        )));
    }
}

#[cfg(test)]
mod tests;
