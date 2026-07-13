//! Anthropic Messages API streaming client — hand-rolled over `reqwest`, no
//! Anthropic SDK crate. Implements [`entanglement_core::Llm`] by POSTing to
//! `/v1/messages` with `stream: true` and parsing the Server-Sent-Events stream
//! into [`LlmEvent`]s (incremental text, assembled tool calls, terminal usage).
//!
//! # Wire shape
//! Anthropic streams `event: <type>\n data: <json>\n\n` frames. We care about:
//! - `message_start`            → input token usage
//! - `content_block_start`      → start a `tool_use` block (id + name)
//! - `content_block_delta`      → `text_delta` (yield text) or
//!   `input_json_delta` (append to the pending tool's JSON input)
//! - `content_block_stop`       → finalize a pending tool → `ToolCall`
//! - `message_delta`            → output token usage
//! - `message_stop`             → `Finish`
//! - `error`                    → mid-stream failure
//!
//! See ADR-0007 for why this lives outside `entanglement-core` (reqwest is a transport
//! dep; core stays pure).

use crate::client::HttpClient;
use async_stream::try_stream;
use async_trait::async_trait;
use entanglement_core::{
    Llm, LlmEvent, LlmRequest, LlmSession, LlmStream, Message, MessageRole, ToolSpec,
};
use futures::StreamExt;
use serde_json::{json, Value};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 16_384;

/// Streaming Anthropic Messages client. Cheap to clone (the HTTP client is
/// `Arc`-shared internally); build one per session via [`anthropic_factory`].
#[derive(Clone)]
pub struct AnthropicLlm {
    api_key: String,
    default_model: String,
    max_tokens: u32,
    http: HttpClient,
}

impl AnthropicLlm {
    pub fn new(
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        http: HttpClient,
    ) -> Self {
        Self::with_max_tokens(api_key, default_model, DEFAULT_MAX_TOKENS, http)
    }

    pub fn with_max_tokens(
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        max_tokens: u32,
        http: HttpClient,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            default_model: default_model.into(),
            max_tokens,
            http,
        }
    }
}

/// Build an [`LlmFactory`] wired to Anthropic. Each session gets its own cloned
/// [`AnthropicLlm`].
pub fn anthropic_factory(
    api_key: impl Into<String>,
    default_model: impl Into<String>,
    http: HttpClient,
) -> entanglement_core::LlmFactory {
    let llm = AnthropicLlm::new(api_key, default_model, http);
    std::sync::Arc::new(move || LlmSession::new(Box::new(llm.clone())))
}

#[async_trait]
impl Llm for AnthropicLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model);
        let body = build_body(model, req.system, req.messages, req.tools, self.max_tokens);

        let response = self
            .http
            .execute_with_retry(ANTHROPIC_API_URL, || {
                self.http
                    .client()
                    .post(ANTHROPIC_API_URL)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
                    .send()
            })
            .await
            .map_err(|e| match e {
                crate::client::RetryError::Permanent(e) => {
                    anyhow::anyhow!("anthropic request failed: {e}")
                }
                crate::client::RetryError::Exhausted(attempts, e) => {
                    anyhow::anyhow!("anthropic request failed after {} attempts: {e}", attempts)
                }
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = crate::client::extract_retry_after_from_response(&response);
            let text = response.text().await.unwrap_or_default();

            if status.as_u16() == 429 {
                if let Some(retry_after) = retry_after {
                    tracing::warn!(retry_after = ?retry_after, "rate limited, backing off");
                    return Err(anyhow::anyhow!(
                        "anthropic rate limited, retry after {:?}",
                        retry_after
                    ));
                }
            }

            anyhow::bail!("anthropic HTTP {status}: {text}");
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, anyhow::Error>>(8);
        tokio::spawn(async move {
            let mut bytes = response.bytes_stream();
            while let Some(item) = bytes.next().await {
                let chunk = item.map_err(|e| anyhow::anyhow!("anthropic stream read: {e}"));
                if tx.send(chunk.map(|c| c.to_vec())).await.is_err() {
                    break;
                }
            }
        });

        let stream = try_stream! {
            let mut buf = String::new();
            let mut current_tool: Option<PendingTool> = None;
            let mut input_tokens: Option<u64> = None;
            let mut output_tokens: Option<u64> = None;
            let mut rx = rx;

            while let Some(item) = rx.recv().await {
                let chunk = item?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(idx) = buf.find("\n\n") {
                    let frame: String = buf.drain(..idx + 2).collect();
                    let (event, data) = parse_frame(&frame);
                    for ev in handle_frame(
                        &event,
                        data,
                        &mut current_tool,
                        &mut input_tokens,
                        &mut output_tokens,
                    )? {
                        yield ev;
                    }
                }
            }
            yield LlmEvent::Finish {
                input_tokens,
                output_tokens,
            };
        };

        tracing::debug!(model = model, "anthropic stream started");
        Ok(stream.boxed())
    }
}

struct PendingTool {
    id: String,
    name: String,
    input_buf: String,
}

// ── request body ────────────────────────────────────────────────────────────

fn build_body(
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    max_tokens: u32,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": convert_messages(messages),
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = json!(convert_tools(tools));
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
                if !messages[i].text.is_empty() {
                    out.push(json!({ "role": "user", "content": messages[i].text }));
                }
                i += 1;
            }
            MessageRole::Assistant => {
                let mut blocks: Vec<Value> = Vec::new();
                if !messages[i].text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": messages[i].text }));
                }
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
                    results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": messages[i].text,
                    }));
                    i += 1;
                }
                if !results.is_empty() {
                    out.push(json!({ "role": "user", "content": results }));
                }
            }
        }
    }
    out
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

// ── SSE frame parsing ───────────────────────────────────────────────────────

/// Split one SSE frame into its `event:` type and parsed `data:` JSON payload.
fn parse_frame(frame: &str) -> (String, Option<Value>) {
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
#[allow(clippy::too_many_arguments)]
fn handle_frame(
    event: &str,
    data: Option<Value>,
    current_tool: &mut Option<PendingTool>,
    input_tokens: &mut Option<u64>,
    output_tokens: &mut Option<u64>,
) -> Result<Vec<LlmEvent>, anyhow::Error> {
    let mut out = Vec::new();
    let data = data.unwrap_or(Value::Null);
    match event {
        "message_start" => {
            if let Some(t) = data
                .pointer("/message/usage/input_tokens")
                .and_then(|v| v.as_u64())
            {
                *input_tokens = Some(t);
            }
        }
        "message_delta" => {
            if let Some(t) = data
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
            {
                *output_tokens = Some(t);
            }
        }
        "content_block_start" => {
            if data.pointer("/content_block/type").and_then(|v| v.as_str()) == Some("tool_use") {
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
                            tool.input_buf.push_str(partial);
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
                out.push(LlmEvent::ToolCall(entanglement_core::ToolCall {
                    id: tool.id,
                    name: tool.name,
                    input,
                }));
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
mod tests {
    use super::*;

    fn msg(role: MessageRole, text: &str) -> Message {
        Message {
            role,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    #[test]
    fn body_omits_tools_when_empty() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
        );
        assert!(body.get("tools").is_none());
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn body_includes_input_schema_when_tools_present() {
        let spec = ToolSpec::new("greet", "say hi");
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[spec],
            1024,
        );
        assert_eq!(body["tools"][0]["name"], "greet");
        assert!(body["tools"][0]["input_schema"].is_object());
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let msgs = vec![
            Message {
                role: MessageRole::Assistant,
                text: "".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Tool,
                text: "r1".into(),
                tool_calls: vec![],
                tool_call_id: Some("a".into()),
            },
            Message {
                role: MessageRole::Tool,
                text: "r2".into(),
                tool_calls: vec![],
                tool_call_id: Some("b".into()),
            },
        ];
        let out = convert_messages(&msgs);
        // assistant (empty text, no calls) is dropped; both results land in one user msg.
        assert_eq!(out.len(), 1);
        let blocks = out[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
    }

    #[test]
    fn text_delta_yields_text() {
        let data = json!({ "delta": { "type": "text_delta", "text": "hel" } });
        let mut tool = None;
        let evs = handle_frame(
            "content_block_delta",
            Some(data),
            &mut tool,
            &mut None,
            &mut None,
        )
        .unwrap();
        assert_eq!(evs, vec![LlmEvent::Text("hel".into())]);
    }

    #[test]
    fn tool_block_assembles_across_deltas() {
        let start = json!({
            "content_block": { "type": "tool_use", "id": "t1", "name": "greet", "input": {} }
        });
        let d1 = json!({ "delta": { "type": "input_json_delta", "partial_json": "{\"nm\":" } });
        let d2 = json!({ "delta": { "type": "input_json_delta", "partial_json": "\"sam\"}" } });

        let mut tool = None;
        let _ = handle_frame(
            "content_block_start",
            Some(start),
            &mut tool,
            &mut None,
            &mut None,
        )
        .unwrap();
        let _ = handle_frame(
            "content_block_delta",
            Some(d1),
            &mut tool,
            &mut None,
            &mut None,
        )
        .unwrap();
        let _ = handle_frame(
            "content_block_delta",
            Some(d2),
            &mut tool,
            &mut None,
            &mut None,
        )
        .unwrap();
        let evs =
            handle_frame("content_block_stop", None, &mut tool, &mut None, &mut None).unwrap();
        assert_eq!(
            evs,
            vec![LlmEvent::ToolCall(entanglement_core::ToolCall {
                id: "t1".into(),
                name: "greet".into(),
                input: r#"{"nm":"sam"}"#.into(),
            })]
        );
    }

    #[test]
    fn usage_is_captured_from_frames() {
        let mut input = None;
        let mut output = None;
        let _ = handle_frame(
            "message_start",
            Some(json!({ "message": { "usage": { "input_tokens": 42 } } })),
            &mut None,
            &mut input,
            &mut output,
        )
        .unwrap();
        let _ = handle_frame(
            "message_delta",
            Some(json!({ "usage": { "output_tokens": 7 } })),
            &mut None,
            &mut input,
            &mut output,
        )
        .unwrap();
        assert_eq!(input, Some(42));
        assert_eq!(output, Some(7));
    }

    #[test]
    fn parse_frame_reads_event_and_data() {
        let frame = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
        let (event, data) = parse_frame(frame);
        assert_eq!(event, "content_block_delta");
        assert_eq!(data.unwrap()["delta"]["text"], "x");
    }
}
