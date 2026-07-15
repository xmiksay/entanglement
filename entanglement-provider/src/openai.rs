//! Generic OpenAI-compatible streaming client — hand-rolled over `reqwest`,
//! no SDK crate. One [`OpenAiLlm`] serves any provider that speaks the
//! `/chat/completions` wire format: **z.ai** (GLM models, entanglement's primary),
//! **OpenAI**, and **Ollama**'s `/v1` compat endpoint. The only differences
//! between them are config: base URL, whether a key is required, and the model
//! name — all injected by the host.
//!
//! Implements [`crate::Llm`] by POSTing to `/chat/completions` with
//! `stream: true` and parsing the Server-Sent-Events stream into [`LlmEvent`]s
//! (incremental text, assembled tool calls, usage).
//!
//! # Preset base URLs
//! - [`ZAI_CODING_PLAN_BASE`] — GLM Coding Plan (dedicated tier), entanglement default.
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
//! `entanglement-core`.

use std::collections::BTreeMap;

use crate::client::HttpClient;
use crate::web_search::WebSearchConfig;
use crate::{
    ContentPart, GenerationParams, ImageSource, Llm, LlmEvent, LlmRequest, LlmStream, Message,
    MessageRole, StopReason, ToolCall, ToolSpec, Usage,
};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

/// z.ai GLM Coding Plan (dedicated tier) — entanglement's default base URL.
pub const ZAI_CODING_PLAN_BASE: &str = "https://api.z.ai/api/coding/paas/v4";
/// z.ai general (pay-as-you-go) tier.
pub const ZAI_GENERAL_BASE: &str = "https://api.z.ai/api/paas/v4";
/// OpenAI.
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";
/// Local Ollama (OpenAI-compatible `/v1`). Keyless.
pub const OLLAMA_BASE: &str = "http://localhost:11434/v1";

/// Streaming OpenAI-compatible client. `api_key = None` skips the
/// `Authorization` header (for keyless backends like local Ollama). Cheap to
/// clone (the HTTP client is `Arc`-shared internally); build one per session via
/// [`openai_factory`].
#[derive(Clone)]
pub struct OpenAiLlm {
    base_url: String,
    api_key: Option<String>,
    default_model: String,
    /// Catalog-provided per-minute budget for this endpoint (`None` = client
    /// default). Threaded into the per-endpoint rate limiter (#241).
    rpm: Option<u32>,
    /// Opt-in provider-side web search (#305): when `Some`, `build_body` requests
    /// the z.ai `web_search` tool. Bound at construction, invisible to core.
    web_search: Option<WebSearchConfig>,
    http: HttpClient,
}

impl OpenAiLlm {
    /// `api_key = None` sends no `Authorization` header (Ollama). A `Some` key is
    /// sent as `Bearer`. `rpm = None` uses the client's default rate-limit budget.
    /// `web_search = Some(..)` requests provider-side web search (#305).
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        web_search: Option<WebSearchConfig>,
        http: HttpClient,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            default_model: default_model.into(),
            rpm,
            web_search,
            http,
        }
    }
}

/// Factory for one per-session [`OpenAiLlm`]. Pass the provider's base URL, an
/// optional key, the default model id, the endpoint's rpm budget, and the opt-in
/// [`WebSearchConfig`] (`None` disables provider-side web search, #305).
pub fn openai_factory(
    base_url: impl Into<String>,
    api_key: Option<String>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    web_search: Option<WebSearchConfig>,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = OpenAiLlm::new(base_url, api_key, default_model, rpm, web_search, http);
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for OpenAiLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        let body = build_body(
            &model,
            req.system,
            req.messages,
            req.tools,
            req.generation,
            self.web_search.as_ref(),
        );
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        tracing::debug!(
            model = %model,
            base = %self.base_url,
            messages_count = req.messages.len(),
            tools_count = req.tools.len(),
            has_tool_role_messages = req.messages.iter().any(|m| m.role == MessageRole::Tool),
            "openai-compat request"
        );
        crate::client::log_request_body("openai", &body);

        let response = self
            .http
            .execute_with_retry(&self.base_url, self.api_key.as_deref(), self.rpm, || {
                let mut request = self.http.client().post(&url);
                if let Some(key) = &self.api_key {
                    request = request.bearer_auth(key);
                }
                request.json(&body).send()
            })
            .await
            .map_err(|e| match e {
                crate::client::RetryError::Permanent(e) => {
                    anyhow::anyhow!("openai-compat request failed: {e}")
                }
                crate::client::RetryError::Exhausted(attempts, e) => anyhow::anyhow!(
                    "openai-compat request failed after {} attempts: {e}",
                    attempts
                ),
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = crate::client::extract_retry_after_from_response(&response);
            let text = response.text().await.unwrap_or_default();
            tracing::error!(status = %status, response = %text, "openai-compat request failed");

            if status.as_u16() == 429 {
                if let Some(retry_after) = retry_after {
                    tracing::warn!(retry_after = ?retry_after, "rate limited, backing off");
                    return Err(anyhow::anyhow!(
                        "openai-compat rate limited, retry after {:?}",
                        retry_after
                    ));
                }
            }

            anyhow::bail!("openai-compat HTTP {status}: {text}");
        }

        // Forward the SSE body with a per-chunk idle-gap watchdog (#241): a long
        // healthy stream runs to completion, a hung one dies within the gap.
        let rx = crate::client::spawn_byte_stream(response, "openai-compat");

        let stream = try_stream! {
            let mut buf = String::new();
            let mut tools: BTreeMap<u32, PendingTool> = BTreeMap::new();
            let mut usage = Usage::default();
            let mut seen_finish_reason: Option<String> = None;
            let mut rx = rx;

            while let Some(item) = rx.recv().await {
                let chunk = item?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(idx) = buf.find('\n') {
                    let line: String = buf.drain(..idx + 1).collect();
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim();
                    if payload == "[DONE]" {
                        continue;
                    }
                    let data: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(fr) = data.pointer("/choices/0/finish_reason").and_then(|v| v.as_str()) {
                        seen_finish_reason = Some(fr.to_string());
                    }

                    for ev in handle_chunk(&data, &mut tools, &mut usage)? {
                        yield ev;
                    }
                }
            }
            let has_pending_tools = !tools.is_empty();
            if has_pending_tools {
                tracing::warn!(
                    finish_reason = seen_finish_reason.as_deref().unwrap_or("none"),
                    pending_tools = tools.len(),
                    "stream ended with pending tools - flushing anyway"
                );
                for (_, t) in std::mem::take(&mut tools) {
                    let should_emit = if t.arguments.is_empty() {
                        true
                    } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t.arguments) {
                        matches!(v, serde_json::Value::Object(_))
                    } else {
                        tracing::warn!(
                            tool = %t.name,
                            args = %t.arguments,
                            "skipping tool call with malformed JSON arguments"
                        );
                        false
                    };

                    if should_emit {
                        yield LlmEvent::ToolCall(t.into_tool_call());
                    }
                }
            }
            // A tool-flush without an explicit finish_reason still means the model
            // wants to run tools; fall back to ToolUse so the reason is never lost.
            let stop_reason = match seen_finish_reason.as_deref() {
                Some(r) => Some(StopReason::from_openai(r)),
                None if has_pending_tools => Some(StopReason::ToolUse),
                None => None,
            };
            yield LlmEvent::Finish { stop_reason, usage };
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
            provider_meta: None,
        }
    }
}

// ── request body ────────────────────────────────────────────────────────────

fn build_body(
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
    // that channel).
    if let Some(g) = generation {
        if let Some(temp) = g.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max) = g.max_output_tokens {
            body["max_tokens"] = json!(max);
        }
    }
    body
}

/// Map entanglement's `Message` history to OpenAI chat format. Tool results become one
/// `role: "tool"` message each (with its `tool_call_id`); assistant tool calls
/// become a `tool_calls` array carrying the raw JSON argument string.
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            MessageRole::User => {
                out.push(json!({ "role": "user", "content": openai_content(&m.content) }));
            }
            MessageRole::Assistant => {
                let mut entry = json!({ "role": "assistant", "content": m.text() });
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
/// multimodal block array (`text` / `image_url` with a `data:` URL, #197/#221).
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
        })
        .collect();
    Value::Array(blocks)
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

// ── SSE chunk handling ──────────────────────────────────────────────────────

/// Map one parsed `data:` chunk to zero or more [`LlmEvent`]s, updating tool
/// assembly + usage state. Pure (no I/O) so it unit-tests directly. Tools flush
/// when `finish_reason == "tool_calls"` is observed (all args already assembled).
fn handle_chunk(
    data: &Value,
    tools: &mut BTreeMap<u32, PendingTool>,
    usage: &mut Usage,
) -> Result<Vec<LlmEvent>, anyhow::Error> {
    let mut out = Vec::new();

    // Usage arrives in the final chunk (empty choices when include_usage is set).
    if let Some(u) = data.get("usage") {
        // OpenAI's `prompt_tokens` *includes* cache-read tokens; split them so
        // each maps to one pricing dimension (#192): the cached portion bills at
        // the cached rate, the remainder at the full input rate.
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|v| v.as_u64());
        if let Some(c) = cached {
            usage.cached_input_tokens = Some(c);
        }
        if let Some(p) = u.get("prompt_tokens").and_then(|v| v.as_u64()) {
            usage.input_tokens = Some(p.saturating_sub(cached.unwrap_or(0)));
        }
        if let Some(c) = u.get("completion_tokens").and_then(|v| v.as_u64()) {
            usage.output_tokens = Some(c);
        }
    }

    // Provider-side web search (#305): z.ai streams the source list as a
    // `web_search` array. Its exact placement in the streaming wire is unverified,
    // so scan defensively at the top level and inside the delta; each source
    // surfaces as one Reasoning line (cited answer text already flows as `Text`).
    emit_web_search(data.get("web_search"), &mut out);
    if let Some(arr) = data.pointer("/choices/0/delta/web_search") {
        emit_web_search(Some(arr), &mut out);
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
        if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(LlmEvent::Reasoning(text.to_string()));
            }
        }
        if let Some(text) = delta.get("reasoning").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(LlmEvent::Reasoning(text.to_string()));
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
                        if !a.is_empty() {
                            entry.arguments.push_str(a);
                            // Surface the raw arg fragment as it streams (#194) so
                            // heads can render file-sized `edit`/`write` inputs
                            // before the assembled `ToolCall` flushes at finish.
                            out.push(LlmEvent::ToolCallDelta {
                                id: entry.id.clone(),
                                name: entry.name.clone(),
                                delta: a.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str());
    tracing::debug!(
        finish_reason,
        has_tools = !tools.is_empty(),
        "openai-compat chunk"
    );
    let flush = finish_reason == Some("tool_calls");
    if flush {
        for (_, t) in std::mem::take(tools) {
            out.push(LlmEvent::ToolCall(t.into_tool_call()));
        }
    }

    Ok(out)
}

/// Render a z.ai `web_search` source array (#305) as one `[web_search] {title} —
/// {url}` [`LlmEvent::Reasoning`] line per entry. A non-array (or an entry with
/// neither title nor link) is ignored — this is a best-effort, defensive surface.
fn emit_web_search(value: Option<&Value>, out: &mut Vec<LlmEvent>) {
    let Some(entries) = value.and_then(|v| v.as_array()) else {
        return;
    };
    for entry in entries {
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let url = entry
            .get("link")
            .or_else(|| entry.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if title.is_empty() && url.is_empty() {
            continue;
        }
        out.push(LlmEvent::Reasoning(format!("[web_search] {title} — {url}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: MessageRole, text: &str) -> Message {
        Message {
            role,
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::text(text)]
            },
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
            None,
            None,
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("tools").is_none());
        // No generation params ⇒ no temperature/max_tokens on the wire.
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be helpful");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn text_only_user_content_stays_a_plain_string() {
        let out = convert_messages(&[msg(MessageRole::User, "hi")]);
        assert_eq!(out[0]["content"], "hi");
    }

    #[test]
    fn user_image_renders_data_url_block() {
        let user = Message::user_content(vec![
            ContentPart::text("look"),
            ContentPart::image("image/png", "AAAA"),
        ]);
        let out = convert_messages(&[user]);
        let content = &out[0]["content"];
        assert_eq!(content[0], json!({ "type": "text", "text": "look" }));
        assert_eq!(
            content[1],
            json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } })
        );
    }

    #[test]
    fn tool_result_with_image_appends_a_user_image_message() {
        // #221: OpenAI's `role: "tool"` message can't hold an image, so the image
        // is handed to the model as a trailing `role: "user"` message with an
        // `image_url` block; the tool message keeps a text placeholder.
        let tool = Message::tool_content("call-1", vec![ContentPart::image("image/png", "AAAA")]);
        let out = convert_messages(&[tool]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "call-1");
        assert_eq!(
            out[0]["content"],
            "[image returned; see the following message]"
        );
        assert_eq!(out[1]["role"], "user");
        assert_eq!(
            out[1]["content"][0],
            json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } })
        );
    }

    #[test]
    fn text_only_tool_result_stays_a_single_string_message() {
        let tool = Message::tool("call-1", "done");
        let out = convert_messages(&[tool]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["content"], "done");
    }

    #[test]
    fn body_includes_tools_with_parameters_schema() {
        let spec = ToolSpec::new("greet", "say hi");
        let body = build_body(
            "glm-5.2",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[spec],
            None,
            None,
        );
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "greet");
        assert!(body["tools"][0]["function"]["parameters"].is_object());
    }

    #[test]
    fn generation_params_set_temperature_and_max_tokens() {
        let body = build_body(
            "glm-5.2",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            Some(GenerationParams {
                temperature: Some(0.7),
                max_output_tokens: Some(2048),
                // No thinking channel on the OpenAI-compat wire — dropped.
                thinking_budget_tokens: Some(4096),
            }),
            None,
        );
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        assert_eq!(body["max_tokens"], 2048);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn generation_params_omit_unset_knobs() {
        let body = build_body(
            "glm-5.2",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            Some(GenerationParams::default()),
            None,
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn tool_results_become_one_message_each() {
        let msgs = vec![Message::tool("a", "r1"), Message::tool("b", "r2")];
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
        let msgs = vec![Message::assistant(
            "thinking",
            vec![ToolCall {
                id: "c1".into(),
                name: "greet".into(),
                input: r#"{"nm":"sam"}"#.into(),
                provider_meta: None,
            }],
        )];
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
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
        assert_eq!(evs, vec![LlmEvent::Text("hel".into())]);
    }

    #[test]
    fn empty_content_delta_emits_nothing() {
        let data = json!({ "choices": [{ "delta": { "content": "" } }] });
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
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

        let _ = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
        assert!(tools.contains_key(&0)); // assembled but not yet flushed
        let _ = handle_chunk(&d2, &mut tools, &mut Usage::default()).unwrap();
        let evs = handle_chunk(&d3, &mut tools, &mut Usage::default()).unwrap();
        assert_eq!(
            evs,
            vec![LlmEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "greet".into(),
                input: r#"{"nm":"sam"}"#.into(),
                provider_meta: None,
            })]
        );
        assert!(tools.is_empty(), "flush should drain the map");
    }

    #[test]
    fn tool_arg_fragments_stream_as_deltas_before_the_assembled_call() {
        // Each `function.arguments` fragment is surfaced as a `ToolCallDelta`
        // (id + name + raw fragment) as it arrives (#194); the concatenated
        // deltas rebuild the eventual `ToolCall::input`.
        let mut tools = BTreeMap::new();
        let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "id": "c1", "type": "function",
              "function": { "name": "greet", "arguments": "{\"nm\":" } }
        ] } }] });
        let d2 = json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "function": { "arguments": "\"sam\"}" } }
        ] } }] });

        let e1 = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
        assert_eq!(
            e1,
            vec![LlmEvent::ToolCallDelta {
                id: "c1".into(),
                name: "greet".into(),
                delta: "{\"nm\":".into(),
            }]
        );
        let e2 = handle_chunk(&d2, &mut tools, &mut Usage::default()).unwrap();
        assert_eq!(
            e2,
            vec![LlmEvent::ToolCallDelta {
                id: "c1".into(),
                name: "greet".into(),
                delta: "\"sam\"}".into(),
            }]
        );
        // Fragments joined equal what a flush would assemble as the input.
        let joined: String = [&e1, &e2]
            .iter()
            .flat_map(|evs| evs.iter())
            .map(|ev| match ev {
                LlmEvent::ToolCallDelta { delta, .. } => delta.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(joined, r#"{"nm":"sam"}"#);
    }

    #[test]
    fn empty_arg_fragment_emits_no_delta() {
        // A tool-call delta that only carries id/name (no args yet) must not
        // emit an empty `ToolCallDelta`.
        let mut tools = BTreeMap::new();
        let d = json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "id": "c1", "type": "function",
              "function": { "name": "greet", "arguments": "" } }
        ] } }] });
        let evs = handle_chunk(&d, &mut tools, &mut Usage::default()).unwrap();
        assert!(evs.is_empty(), "no args ⇒ no delta: {evs:?}");
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
        let _ = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
        let evs = handle_chunk(&d2, &mut tools, &mut Usage::default()).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(
            evs[0],
            LlmEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "a".into(),
                input: "{}".into(),
                provider_meta: None,
            })
        );
        assert_eq!(
            evs[1],
            LlmEvent::ToolCall(ToolCall {
                id: "c2".into(),
                name: "b".into(),
                input: "{}".into(),
                provider_meta: None,
            })
        );
    }

    #[test]
    fn usage_is_captured_from_chunk() {
        let mut usage = Usage::default();
        let data = json!({ "choices": [], "usage": {
            "prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49
        } });
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut usage).unwrap();
        assert!(evs.is_empty()); // no content/tool event from a usage-only chunk
        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.cached_input_tokens, None);
    }

    #[test]
    fn cached_prompt_tokens_split_out_of_input() {
        // OpenAI reports cached reads inside `prompt_tokens`; the input count is
        // the remainder so each dimension prices once (#192).
        let mut usage = Usage::default();
        let data = json!({ "choices": [], "usage": {
            "prompt_tokens": 100, "completion_tokens": 8, "total_tokens": 108,
            "prompt_tokens_details": { "cached_tokens": 30 }
        } });
        let _ = handle_chunk(&data, &mut BTreeMap::new(), &mut usage).unwrap();
        assert_eq!(usage.input_tokens, Some(70));
        assert_eq!(usage.cached_input_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(8));
    }

    #[test]
    fn stop_finish_reason_does_not_flush_or_error() {
        let mut tools = BTreeMap::new();
        let data = json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        let evs = handle_chunk(&data, &mut tools, &mut Usage::default()).unwrap();
        assert!(evs.is_empty());
        assert!(tools.is_empty());
    }

    #[test]
    fn stream_without_finish_reason_flushes_pending_tools() {
        let mut tools = BTreeMap::new();
        let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "id": "c1", "type": "function",
              "function": { "name": "greet", "arguments": "{\"nm\":\"sam\"}" } }
        ] } }] });
        let _ = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
        assert!(tools.contains_key(&0), "tool should be assembled");

        // Simulate stream ending without explicit finish_reason - this would be
        // handled by the flush-at-end logic in the streaming loop
        assert!(!tools.is_empty(), "tools should still be pending");
    }

    // ── provider-side web search (#305) ─────────────────────────────────────

    #[test]
    fn body_omits_web_search_tool_without_config() {
        let body = build_body(
            "glm-5.2",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            None,
            None,
        );
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn body_pushes_web_search_tool_when_configured() {
        let ws = WebSearchConfig {
            enabled: true,
            max_uses: Some(3),
            allowed_domains: vec!["docs.rs".into(), "example.com".into()],
        };
        let body = build_body(
            "glm-5.2",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            None,
            Some(&ws),
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(tools[0]["web_search"]["enable"], true);
        assert_eq!(tools[0]["web_search"]["search_result"], true);
        assert_eq!(tools[0]["web_search"]["count"], 3);
        assert_eq!(
            tools[0]["web_search"]["search_domain_filter"],
            "docs.rs,example.com"
        );
    }

    #[test]
    fn web_search_tool_rides_alongside_function_tools() {
        let ws = WebSearchConfig {
            enabled: true,
            max_uses: None,
            allowed_domains: vec![],
        };
        let body = build_body(
            "glm-5.2",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[ToolSpec::new("greet", "say hi")],
            None,
            Some(&ws),
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[1]["type"], "web_search");
        // No knobs set ⇒ no `count` / `search_domain_filter`.
        assert!(tools[1]["web_search"].get("count").is_none());
        assert!(tools[1]["web_search"].get("search_domain_filter").is_none());
    }

    #[test]
    fn web_search_array_surfaces_as_reasoning() {
        // A chunk carrying a `web_search` source array (defensive top-level
        // placement) yields one Reasoning line per entry, no Text/ToolCall.
        let data = json!({ "web_search": [
            { "title": "Rust async", "link": "https://docs.rs/async" },
            { "title": "Tokio", "url": "https://tokio.rs" },
        ] });
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
        assert_eq!(
            evs,
            vec![
                LlmEvent::Reasoning("[web_search] Rust async — https://docs.rs/async".into()),
                LlmEvent::Reasoning("[web_search] Tokio — https://tokio.rs".into()),
            ]
        );
    }

    #[test]
    fn chunk_without_web_search_array_emits_no_reasoning() {
        let data = json!({ "choices": [{ "delta": { "content": "hi" } }] });
        let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
        assert_eq!(evs, vec![LlmEvent::Text("hi".into())]);
    }
}
