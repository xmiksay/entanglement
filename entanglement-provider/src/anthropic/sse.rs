//! Server-Sent-Events frame parsing: one Anthropic SSE frame → zero or more
//! [`LlmEvent`]s. Split out of `anthropic/mod.rs` to keep the streaming client
//! itself under the file-size cap.
//!
//! # Wire shape
//! - `message_start`            → input token usage
//! - `content_block_start`      → start a `tool_use`/`thinking` block, or a
//!   complete `redacted_thinking` block
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
}

/// An extended-thinking block being assembled across deltas. Both halves arrive
/// incrementally and on separate delta types — `thinking_delta` for the text,
/// `signature_delta` for the integrity signature — so neither is complete until
/// `content_block_stop`.
#[derive(Default)]
pub(super) struct PendingThinking {
    /// Human-readable reasoning. Legitimately empty on current models, which
    /// omit thinking text by default while still signing the block.
    pub text: String,
    /// Opaque signature Anthropic validates on replay. A block without one
    /// cannot be sent back.
    pub signature: String,
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
/// Extended-thinking blocks are captured whole — signature included — as a
/// `ContentBlock`, the block Anthropic requires back on a tool round-trip.
pub(super) fn handle_frame(
    event: &str,
    data: Option<Value>,
    current_tool: &mut Option<PendingTool>,
    current_thinking: &mut Option<PendingThinking>,
    usage: &mut Usage,
    stop_reason: &mut Option<StopReason>,
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
            }
        }
        "content_block_start" => {
            let block_type = data.pointer("/content_block/type").and_then(|v| v.as_str());
            match block_type {
                Some("tool_use") => {
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
                    });
                }
                // Extended thinking: text and signature both arrive as deltas, so
                // accumulate until `content_block_stop`. Started even when the
                // model returns no thinking text at all (current models omit it by
                // default) — the signature alone still makes the block replayable,
                // and dropping it would break the next tool round-trip.
                Some("thinking") => {
                    *current_thinking = Some(PendingThinking::default());
                }
                // Redacted thinking arrives whole: opaque `data`, no text to
                // show, and it must be echoed back verbatim alongside its
                // siblings.
                Some("redacted_thinking") => {
                    if let Some(block) = data.pointer("/content_block") {
                        out.push(LlmEvent::ContentBlock(ContentPart::reasoning(
                            "anthropic",
                            "",
                            block.clone(),
                        )));
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
                                // assembled `ToolCall`.
                                out.push(LlmEvent::ToolCallDelta {
                                    id: tool.id.clone(),
                                    name: tool.name.clone(),
                                    delta: partial.to_string(),
                                });
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                            out.push(LlmEvent::Reasoning(thinking.to_string()));
                            if let Some(p) = current_thinking.as_mut() {
                                p.text.push_str(thinking);
                            }
                        }
                    }
                    Some("signature_delta") => {
                        // The block's integrity signature — not display content,
                        // but load-bearing on replay: Anthropic rejects a thinking
                        // block whose signature is missing or altered, so it is
                        // accumulated here rather than dropped.
                        if let Some(sig) = delta.get("signature").and_then(|t| t.as_str()) {
                            if let Some(p) = current_thinking.as_mut() {
                                p.signature.push_str(sig);
                            }
                        }
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
                out.push(LlmEvent::ToolCall(crate::ToolCall {
                    id: tool.id,
                    name: tool.name,
                    input,
                    provider_meta: None,
                }));
            } else if let Some(thinking) = current_thinking.take() {
                // Rebuild the block in Anthropic's own wire shape so replay can
                // send it back byte-for-byte. Empty `thinking` text is normal and
                // still replayable; a block with no signature is not — Anthropic
                // rejects it — so that one is captured for display only.
                let block = json!({
                    "type": "thinking",
                    "thinking": thinking.text,
                    "signature": thinking.signature,
                });
                if !thinking.signature.is_empty() {
                    out.push(LlmEvent::ContentBlock(ContentPart::reasoning(
                        "anthropic",
                        thinking.text,
                        block,
                    )));
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

#[cfg(test)]
mod tests;
