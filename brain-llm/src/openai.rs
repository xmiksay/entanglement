//! Generic OpenAI-compatible streaming client — hand-rolled over `reqwest`,
//! no SDK crate. One [`OpenAiLlm`] serves any provider that speaks the
//! `/chat/completions` wire format: **z.ai** (GLM models, brain's primary),
//! **OpenAI**, and **Ollama**'s `/v1` compat endpoint. The only differences
//! between them are config: base URL, whether a key is required, and the model
//! name — all injected by the host.
//!
//! Implements [`brain_core::Llm`] by POSTing to `/chat/completions` with
//! `stream: true` and parsing the Server-Sent-Events stream into [`LlmEvent`]s
//! (incremental text, assembled tool calls, usage).
//!
//! # Preset base URLs
//! - [`ZAI_CODING_PLAN_BASE`] — GLM Coding Plan (dedicated tier), brain default.
//! - [`ZAI_GENERAL_BASE`] — z.ai pay-as-you-go.
//! - [`OPENAI_BASE`] — OpenAI.
//! - [`OLLAMA_BASE`] — local Ollama (keyless).
//!
//! # Wire shape (OpenAI Chat Completions streaming)
//! Frames are `data: <json>\n\n`; the stream ends with `data: [DONE]`. Per chunk:
//! - `choices[0].delta.content`        → incremental text
//! - `choices[0].delta.tool_calls[]`   → per-index tool assembly (`id` +
//!   `function.name` on the first delta, `function.arguments` appended after)
//! - `choices[0].finish_reason`        → `"stop"` (text done) or `"tool_calls"`
//!   (flush every assembled tool as a [`LlmEvent::ToolCall`])
//! - `usage` (final chunk)             → token counts
//!
//! Tool-result messages round-trip as `role: "tool"` **per call** — unlike
//! Anthropic, which merges consecutive results into one user turn (that's why
//! Anthropic keeps its own module). See ADR-0007 for why backends live outside
//! `brain-core`.

use std::collections::BTreeMap;
use std::time::Duration;

use async_stream::try_stream;
use async_trait::async_trait;
use brain_core::{Llm, LlmEvent, LlmRequest, LlmStream, Message, MessageRole, ToolCall, ToolSpec};
use futures::StreamExt;
use serde_json::{json, Value};

/// z.ai GLM Coding Plan (dedicated tier) — brain's default base URL.
pub const ZAI_CODING_PLAN_BASE: &str = "https://api.z.ai/api/coding/paas/v4";
/// z.ai general (pay-as-you-go) tier.
pub const ZAI_GENERAL_BASE: &str = "https://api.z.ai/api/paas/v4";
/// OpenAI.
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";
/// Local Ollama (OpenAI-compatible `/v1`). Keyless.
pub const OLLAMA_BASE: &str = "http://localhost:11434/v1";

const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

/// Streaming OpenAI-compatible client. `api_key = None` skips the
/// `Authorization` header (for keyless backends like local Ollama). Cheap to
/// clone (the HTTP client is `Arc`-shared internally); build one per session via
/// [`openai_factory`].
#[derive(Clone)]
pub struct OpenAiLlm {
    base_url: String,
    api_key: Option<String>,
    default_model: String,
    http: reqwest::Client,
}

impl OpenAiLlm {
    /// `api_key = None` sends no `Authorization` header (Ollama). A `Some` key is
    /// sent as `Bearer`.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: impl Into<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("failed to build reqwest client for OpenAI-compatible backend");
        Self {
            base_url: base_url.into(),
            api_key,
            default_model: default_model.into(),
            http,
        }
    }
}

/// Factory for one per-session [`OpenAiLlm`]. Pass the provider's base URL, an
/// optional key, and the default model id.
pub fn openai_factory(
    base_url: impl Into<String>,
    api_key: Option<String>,
    default_model: impl Into<String>,
) -> brain_core::LlmFactory {
    let llm = OpenAiLlm::new(base_url, api_key, default_model);
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for OpenAiLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        let body = build_body(&model, req.system, req.messages, req.tools);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut request = self.http.post(&url);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("openai-compat request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("openai-compat HTTP {status}: {text}");
        }

        // reqwest's `bytes_stream` borrows the response, so it isn't `'static`.
        // Drain it on a detached task into an owned-byte mpsc; the consumer owns
        // only the receiver and is thus `'static`.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, anyhow::Error>>(8);
        tokio::spawn(async move {
            let mut bytes = response.bytes_stream();
            while let Some(item) = bytes.next().await {
                let chunk = item.map_err(|e| anyhow::anyhow!("openai-compat stream read: {e}"));
                if tx.send(chunk.map(|c| c.to_vec())).await.is_err() {
                    break; // consumer gone
                }
            }
        });

        let stream = try_stream! {
            let mut buf = String::new();
            // Per-index tool assembly. BTreeMap so flush order is stable (by index).
            let mut tools: BTreeMap<u32, PendingTool> = BTreeMap::new();
            let mut input_tokens: Option<u64> = None;
            let mut output_tokens: Option<u64> = None;
            let mut rx = rx;

            while let Some(item) = rx.recv().await {
                let chunk = item?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // Process whole lines (handles `\r\n` and `\n\n` separators).
                while let Some(idx) = buf.find('\n') {
                    let line: String = buf.drain(..idx + 1).collect();
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue; // SSE comments / event lines we don't use
                    };
                    let payload = payload.trim();
                    if payload == "[DONE]" {
                        // Stream terminator. Usage precedes [DONE] and is already
                        // captured; the terminal Finish is emitted after this loop.
                        continue;
                    }
                    let data: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue, // tolerate stray keepalive payloads
                    };
                    for ev in handle_chunk(&data, &mut tools, &mut input_tokens, &mut output_tokens)? {
                        yield ev;
                    }
                }
            }
            // Connection closed (with or without `[DONE]`). Flush any tools that
            // arrived without an explicit finish_reason flush, then end the turn.
            for (_, t) in std::mem::take(&mut tools) {
                yield LlmEvent::ToolCall(t.into_tool_call());
            }
            yield LlmEvent::Finish {
                input_tokens,
                output_tokens,
            };
        };

        tracing::debug!(model = %model, base = %self.base_url, "openai-compat stream started");
        Ok(stream.boxed())
    }
}

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

impl PendingTool {
    fn into_tool_call(self) -> ToolCall {
        let input = if self.arguments.is_empty() {
            "{}".to_string()
        } else {
            self.arguments
        };
        ToolCall {
            id: self.id,
            name: self.name,
            input,
        }
    }
}

// ── request body ────────────────────────────────────────────────────────────

fn build_body(model: &str, system: &str, messages: &[Message], tools: &[ToolSpec]) -> Value {
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
    if !tools.is_empty() {
        body["tools"] = json!(convert_tools(tools));
    }
    body
}

/// Map brain's `Message` history to OpenAI chat format. Tool results become one
/// `role: "tool"` message each (with its `tool_call_id`); assistant tool calls
/// become a `tool_calls` array carrying the raw JSON argument string.
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            MessageRole::User => {
                out.push(json!({ "role": "user", "content": m.text }));
            }
            MessageRole::Assistant => {
                let mut entry = json!({ "role": "assistant" });
                if !m.text.is_empty() {
                    entry["content"] = json!(m.text);
                }
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
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.text,
                }));
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

// ── SSE chunk handling ──────────────────────────────────────────────────────

/// Map one parsed `data:` chunk to zero or more [`LlmEvent`]s, updating tool
/// assembly + usage state. Pure (no I/O) so it unit-tests directly. Tools flush
/// when `finish_reason == "tool_calls"` is observed (all args already assembled).
fn handle_chunk(
    data: &Value,
    tools: &mut BTreeMap<u32, PendingTool>,
    input_tokens: &mut Option<u64>,
    output_tokens: &mut Option<u64>,
) -> Result<Vec<LlmEvent>, anyhow::Error> {
    let mut out = Vec::new();

    // Usage arrives in the final chunk (empty choices when include_usage is set).
    if let Some(u) = data.get("usage") {
        if let Some(p) = u.get("prompt_tokens").and_then(|v| v.as_u64()) {
            *input_tokens = Some(p);
        }
        if let Some(c) = u.get("completion_tokens").and_then(|v| v.as_u64()) {
            *output_tokens = Some(c);
        }
    }

    let Some(choice) = data.pointer("/choices/0") else {
        return Ok(out);
    };

    if let Some(delta) = choice.get("delta") {
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(LlmEvent::Text(text.to_string()));
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                let Some(index) = tc.get("index").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let index = index as u32;
                let entry = tools.entry(index).or_default();
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    entry.id = id.to_string();
                }
                if let Some(func) = tc.get("function") {
                    if let Some(n) = func.get("name").and_then(|v| v.as_str()) {
                        entry.name = n.to_string();
                    }
                    if let Some(a) = func.get("arguments").and_then(|v| v.as_str()) {
                        entry.arguments.push_str(a);
                    }
                }
            }
        }
    }

    let flush = choice.get("finish_reason").and_then(|v| v.as_str()) == Some("tool_calls");
    if flush {
        for (_, t) in std::mem::take(tools) {
            out.push(LlmEvent::ToolCall(t.into_tool_call()));
        }
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
    fn body_prepends_system_message_and_omits_tools_when_empty() {
        let body = build_body(
            "glm-5.2",
            "be helpful",
            &[msg(MessageRole::User, "hi")],
            &[],
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("tools").is_none());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be helpful");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn body_includes_tools_with_parameters_schema() {
        let spec = ToolSpec::new("greet", "say hi");
        let body = build_body("glm-5.2", "sys", &[msg(MessageRole::User, "hi")], &[spec]);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "greet");
        assert!(body["tools"][0]["function"]["parameters"].is_object());
    }

    #[test]
    fn tool_results_become_one_message_each() {
        let msgs = vec![
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
        // Unlike Anthropic, two tool results are two messages, not one.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "a");
        assert_eq!(out[0]["content"], "r1");
        assert_eq!(out[1]["tool_call_id"], "b");
    }

    #[test]
    fn assistant_with_tool_calls_serializes_arguments() {
        let msgs = vec![Message {
            role: MessageRole::Assistant,
            text: "thinking".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "greet".into(),
                input: r#"{"nm":"sam"}"#.into(),
            }],
            tool_call_id: None,
        }];
        let out = convert_messages(&msgs);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["content"], "thinking");
        let call = &out[0]["tool_calls"][0];
        assert_eq!(call["id"], "c1");
        assert_eq!(call["function"]["name"], "greet");
        assert_eq!(call["function"]["arguments"], r#"{"nm":"sam"}"#);
    }

    #[test]
    fn text_delta_yields_text() {
        let data = json!({ "choices": [{ "delta": { "content": "hel" } }] });
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut None, &mut None).unwrap();
        assert_eq!(evs, vec![LlmEvent::Text("hel".into())]);
    }

    #[test]
    fn empty_content_delta_emits_nothing() {
        let data = json!({ "choices": [{ "delta": { "content": "" } }] });
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut None, &mut None).unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn tool_calls_assemble_across_deltas_and_flush_on_finish() {
        let mut tools = BTreeMap::new();
        let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "id": "c1", "type": "function",
              "function": { "name": "greet", "arguments": "{\"nm\":" } }
        ] } }] });
        let d2 = json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "function": { "arguments": "\"sam\"}" } }
        ] } }] });
        let d3 = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });

        let _ = handle_chunk(&d1, &mut tools, &mut None, &mut None).unwrap();
        assert!(tools.contains_key(&0)); // assembled but not yet flushed
        let _ = handle_chunk(&d2, &mut tools, &mut None, &mut None).unwrap();
        let evs = handle_chunk(&d3, &mut tools, &mut None, &mut None).unwrap();
        assert_eq!(
            evs,
            vec![LlmEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "greet".into(),
                input: r#"{"nm":"sam"}"#.into(),
            })]
        );
        assert!(tools.is_empty(), "flush should drain the map");
    }

    #[test]
    fn multiple_tools_flush_in_index_order() {
        let mut tools = BTreeMap::new();
        let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 1, "id": "c2", "type": "function",
              "function": { "name": "b", "arguments": "{}" } },
            { "index": 0, "id": "c1", "type": "function",
              "function": { "name": "a", "arguments": "{}" } }
        ] } }] });
        let d2 = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
        let _ = handle_chunk(&d1, &mut tools, &mut None, &mut None).unwrap();
        let evs = handle_chunk(&d2, &mut tools, &mut None, &mut None).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(
            evs[0],
            LlmEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "a".into(),
                input: "{}".into()
            })
        );
        assert_eq!(
            evs[1],
            LlmEvent::ToolCall(ToolCall {
                id: "c2".into(),
                name: "b".into(),
                input: "{}".into()
            })
        );
    }

    #[test]
    fn usage_is_captured_from_chunk() {
        let mut input = None;
        let mut output = None;
        let data = json!({ "choices": [], "usage": {
            "prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49
        } });
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut input, &mut output).unwrap();
        assert!(evs.is_empty()); // no content/tool event from a usage-only chunk
        assert_eq!(input, Some(42));
        assert_eq!(output, Some(7));
    }

    #[test]
    fn stop_finish_reason_does_not_flush_or_error() {
        let mut tools = BTreeMap::new();
        let data = json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        let evs = handle_chunk(&data, &mut tools, &mut None, &mut None).unwrap();
        assert!(evs.is_empty());
        assert!(tools.is_empty());
    }
}
