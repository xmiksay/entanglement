//! Anthropic Messages API streaming client — hand-rolled over `reqwest`, no
//! Anthropic SDK crate. Implements [`crate::Llm`] by POSTing to
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
//! The `Llm` trait + its DTOs live in this crate (the leaf); `entanglement-core`
//! depends on it and drives `dyn Llm` from the engine loop (ADR-0053, which
//! inverted the original trait-in-core seam of ADR-0006 / ADR-0007).

use crate::client::HttpClient;
use crate::web_search::WebSearchConfig;
use crate::{
    ContentPart, GenerationParams, ImageSource, Llm, LlmEvent, LlmRequest, LlmStream, Message,
    MessageRole, ReasoningEffort, StopReason, ToolSpec, Usage,
};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Fallback output cap when the request carries no
/// [`GenerationParams::max_output_tokens`] (Anthropic *requires* `max_tokens`).
const DEFAULT_MAX_TOKENS: u32 = 16_384;
/// Thinking-budget tokens for [`ReasoningEffort::High`] when the request sets no
/// explicit [`GenerationParams::thinking_budget_tokens`] (#374) — Anthropic has
/// no effort concept of its own, so `reasoning_effort` maps onto a thinking
/// tier here instead.
const HIGH_EFFORT_THINKING_BUDGET: u32 = 32_000;
/// Thinking-budget tokens for [`ReasoningEffort::Medium`] (#374).
const MEDIUM_EFFORT_THINKING_BUDGET: u32 = 8_000;

/// Streaming Anthropic Messages client. Cheap to clone (the HTTP client is
/// `Arc`-shared internally); build one per session via [`anthropic_factory`].
#[derive(Clone)]
pub struct AnthropicLlm {
    api_key: String,
    default_model: String,
    /// Fallback output cap ([`DEFAULT_MAX_TOKENS`]) used only when a request omits
    /// its own [`GenerationParams::max_output_tokens`] (#191).
    default_max_tokens: u32,
    /// Catalog-provided per-minute budget for this endpoint (`None` = client
    /// default). Threaded into the per-endpoint rate limiter (#241).
    rpm: Option<u32>,
    /// Catalog-provided in-flight concurrency cap for this endpoint (`None` =
    /// client default). Threaded into the per-endpoint concurrency permit (#414).
    concurrency: Option<usize>,
    /// Opt-in provider-side web search (#305): when `Some`, `build_body` requests
    /// the `web_search_20250305` server tool. Bound at construction, invisible to
    /// core.
    web_search: Option<WebSearchConfig>,
    http: HttpClient,
}

impl AnthropicLlm {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        web_search: Option<WebSearchConfig>,
        http: HttpClient,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            default_model: default_model.into(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
            rpm,
            concurrency,
            web_search,
            http,
        }
    }
}

/// Build an [`LlmFactory`] wired to Anthropic. Each session gets its own cloned
/// [`AnthropicLlm`]. `rpm`/`concurrency = None` use the client's defaults;
/// `web_search = Some(..)` requests provider-side web search (#305).
pub fn anthropic_factory(
    api_key: impl Into<String>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    web_search: Option<WebSearchConfig>,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = AnthropicLlm::new(api_key, default_model, rpm, concurrency, web_search, http);
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for AnthropicLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model);
        let body = build_body(
            model,
            req.system,
            req.messages,
            req.tools,
            self.default_max_tokens,
            req.generation,
            self.web_search.as_ref(),
        );

        tracing::debug!(
            model = %model,
            messages_count = req.messages.len(),
            tools_count = req.tools.len(),
            "anthropic request"
        );
        crate::client::log_request_body("anthropic", &body);

        let (response, guard) = self
            .http
            .execute_with_retry(
                ANTHROPIC_API_URL,
                Some(&self.api_key),
                self.rpm,
                self.concurrency,
                || {
                    self.http
                        .client()
                        .post(ANTHROPIC_API_URL)
                        .header("x-api-key", &self.api_key)
                        .header("anthropic-version", ANTHROPIC_VERSION)
                        .json(&body)
                        .send()
                },
            )
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

        // Forward the SSE body with a per-chunk idle-gap watchdog (#241): a long
        // healthy stream runs to completion, a hung one dies within the gap.
        let rx = crate::client::spawn_byte_stream(response, "anthropic", guard);

        let stream = try_stream! {
            let mut buf = String::new();
            let mut current_tool: Option<PendingTool> = None;
            let mut usage = Usage::default();
            let mut stop_reason: Option<StopReason> = None;
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
                        &mut usage,
                        &mut stop_reason,
                    )? {
                        yield ev;
                    }
                }
            }
            yield LlmEvent::Finish { stop_reason, usage };
        };

        tracing::debug!(model = model, "anthropic stream started");
        Ok(stream.boxed())
    }
}

struct PendingTool {
    id: String,
    name: String,
    input_buf: String,
    /// `true` for a `server_tool_use` block (provider-side web search, #305):
    /// the provider runs it, so on stop it surfaces as `Reasoning`, **never** a
    /// `ToolCall`, and its arg fragments are not streamed as `ToolCallDelta`.
    is_server: bool,
}

// ── request body ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn build_body(
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    default_max_tokens: u32,
    generation: Option<GenerationParams>,
    web_search: Option<&WebSearchConfig>,
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
        tool_entries.push(web_search_tool_entry(ws));
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
            max_tokens = budget.saturating_add(DEFAULT_MAX_TOKENS);
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

/// Render a message's content parts to Anthropic content blocks (`text` /
/// `image` with a base64 source, #197/#221).
fn anthropic_blocks(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => json!({ "type": "text", "text": text }),
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            } => json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            }),
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
/// `{"type":"web_search_20250305","name":"web_search"}` plus the optional
/// `max_uses` / `allowed_domains` knobs. (`_20260209` needs 4.6+ models — a
/// follow-up behind a `ModelEntry` capability flag.)
fn web_search_tool_entry(ws: &WebSearchConfig) -> Value {
    let mut entry = json!({
        "type": "web_search_20250305",
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
                // A client tool-call block (assembled, then flushed as `ToolCall`)
                // or a provider-side `server_tool_use` block (#305): the latter is
                // executed by the provider, so on stop it surfaces as `Reasoning`,
                // never a `ToolCall`.
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
                // as a complete `web_search_tool_result` block (#305); render each
                // entry as a `Reasoning` line.
                Some("web_search_tool_result") => {
                    emit_web_search_result(data.pointer("/content_block/content"), &mut out);
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
                if tool.is_server {
                    // Provider-side web search (#305): surface the query as a
                    // Reasoning line; the provider runs the search itself, so this
                    // must never become a `ToolCall`.
                    let query = serde_json::from_str::<Value>(&tool.input_buf)
                        .ok()
                        .and_then(|v| v.get("query").and_then(|q| q.as_str()).map(str::to_string))
                        .unwrap_or_default();
                    out.push(LlmEvent::Reasoning(format!("[web_search] {query}")));
                } else {
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

/// Render a `web_search_tool_result` block's `content` (#305) as `Reasoning`
/// lines: an array of `web_search_result` entries → one `[web_search] {title} —
/// {url}` line each; an error object → a single `[web_search] error: {code}` line.
fn emit_web_search_result(content: Option<&Value>, out: &mut Vec<LlmEvent>) {
    let Some(content) = content else {
        return;
    };
    if let Some(entries) = content.as_array() {
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
            out.push(LlmEvent::Reasoning(format!("[web_search] {title} — {url}")));
        }
    } else if content.get("type").and_then(|v| v.as_str()) == Some("web_search_tool_result_error") {
        let code = content
            .get("error_code")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        out.push(LlmEvent::Reasoning(format!("[web_search] error: {code}")));
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
    fn body_omits_tools_when_empty() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            None,
            None,
        );
        assert!(body.get("tools").is_none());
        assert_eq!(body["stream"], true);
        // No request params ⇒ the client's fallback cap, no temperature/thinking.
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("temperature").is_none());
        assert!(body.get("thinking").is_none());
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
            None,
            None,
        );
        assert_eq!(body["tools"][0]["name"], "greet");
        assert!(body["tools"][0]["input_schema"].is_object());
    }

    #[test]
    fn generation_max_output_tokens_overrides_fallback() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: Some(0.3),
                max_output_tokens: Some(8000),
                thinking_budget_tokens: None,
                reasoning_effort: None,
            }),
            None,
        );
        assert_eq!(body["max_tokens"], 8000);
        assert!((body["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn thinking_budget_enables_thinking_and_drops_temperature() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: Some(0.7),
                max_output_tokens: Some(20_000),
                thinking_budget_tokens: Some(10_000),
                reasoning_effort: None,
            }),
            None,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 10_000);
        assert_eq!(body["max_tokens"], 20_000);
        // With thinking on, temperature must be its default — omitted, not sent.
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn thinking_budget_bumps_max_tokens_when_it_would_swallow_the_cap() {
        // Anthropic requires budget_tokens < max_tokens; a budget at/over the cap
        // must lift the cap rather than send an invalid request.
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: None,
                max_output_tokens: Some(4000),
                thinking_budget_tokens: Some(4000),
                reasoning_effort: None,
            }),
            None,
        );
        let max = body["max_tokens"].as_u64().unwrap();
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!(max > budget, "max_tokens {max} must exceed budget {budget}");
    }

    #[test]
    fn high_reasoning_effort_enables_thinking_at_the_tier_default_budget() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: Some(0.7),
                max_output_tokens: None,
                thinking_budget_tokens: None,
                reasoning_effort: Some(ReasoningEffort::High),
            }),
            None,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(
            body["thinking"]["budget_tokens"],
            HIGH_EFFORT_THINKING_BUDGET
        );
        // Thinking on ⇒ temperature omitted, same as an explicit budget.
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn medium_reasoning_effort_uses_a_smaller_tier_budget() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: None,
                max_output_tokens: None,
                thinking_budget_tokens: None,
                reasoning_effort: Some(ReasoningEffort::Medium),
            }),
            None,
        );
        assert_eq!(
            body["thinking"]["budget_tokens"],
            MEDIUM_EFFORT_THINKING_BUDGET
        );
    }

    #[test]
    fn low_reasoning_effort_leaves_thinking_off() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: Some(0.4),
                max_output_tokens: None,
                thinking_budget_tokens: None,
                reasoning_effort: Some(ReasoningEffort::Low),
            }),
            None,
        );
        assert!(body.get("thinking").is_none());
        assert!((body["temperature"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn explicit_thinking_budget_wins_over_reasoning_effort() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: None,
                max_output_tokens: Some(50_000),
                thinking_budget_tokens: Some(1234),
                reasoning_effort: Some(ReasoningEffort::High),
            }),
            None,
        );
        assert_eq!(body["thinking"]["budget_tokens"], 1234);
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let msgs = vec![
            Message::assistant("", vec![]),
            Message::tool("a", "r1"),
            Message::tool("b", "r2"),
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
    fn adjacent_user_turns_coalesce_into_one() {
        // The ambiguous-stop retry shape (ADR-0118): an empty assistant round is
        // dropped, leaving the original prompt adjacent to the injected nudge.
        // Anthropic rejects non-alternating roles, so they must merge.
        let msgs = vec![
            msg(MessageRole::User, "do it"),
            msg(MessageRole::Assistant, ""), // empty ambiguous round → dropped
            msg(MessageRole::User, "[system] nudge"),
        ];
        let out = convert_messages(&msgs);
        assert_eq!(out.len(), 1, "the two user turns must merge; got {out:?}");
        assert_eq!(out[0]["role"], "user");
        let blocks = out[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "do it");
        assert_eq!(blocks[1]["text"], "[system] nudge");
    }

    #[test]
    fn alternating_roles_are_left_untouched() {
        // A well-formed history (the non-empty ambiguous case) must not merge.
        let msgs = vec![
            msg(MessageRole::User, "do it"),
            msg(MessageRole::Assistant, "partial"),
            msg(MessageRole::User, "[system] nudge"),
        ];
        let out = convert_messages(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[2]["role"], "user");
    }

    #[test]
    fn user_image_renders_image_block() {
        let user = Message::user_content(vec![
            ContentPart::text("look"),
            ContentPart::image("image/png", "AAAA"),
        ]);
        let out = convert_messages(&[user]);
        let blocks = out[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0], json!({ "type": "text", "text": "look" }));
        assert_eq!(
            blocks[1],
            json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" },
            })
        );
    }

    #[test]
    fn tool_result_with_image_renders_block_array() {
        // #221: `read` on an image emits an image tool result; text-only results
        // stay plain strings (asserted by `consecutive_tool_results_…`).
        let tool = Message::tool_content("a", vec![ContentPart::image("image/png", "AAAA")]);
        let out = convert_messages(&[tool]);
        let result = &out[0]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "a");
        assert_eq!(
            result["content"][0],
            json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" },
            })
        );
    }

    #[test]
    fn text_delta_yields_text() {
        let data = json!({ "delta": { "type": "text_delta", "text": "hel" } });
        let mut tool = None;
        let evs = handle_frame(
            "content_block_delta",
            Some(data),
            &mut tool,
            &mut Usage::default(),
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
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        let _ = handle_frame(
            "content_block_delta",
            Some(d1),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        let _ = handle_frame(
            "content_block_delta",
            Some(d2),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        let evs = handle_frame(
            "content_block_stop",
            None,
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            evs,
            vec![LlmEvent::ToolCall(crate::ToolCall {
                id: "t1".into(),
                name: "greet".into(),
                input: r#"{"nm":"sam"}"#.into(),
                provider_meta: None,
            })]
        );
    }

    #[test]
    fn input_json_deltas_stream_as_tool_call_deltas() {
        // Each `input_json_delta` is surfaced as a `ToolCallDelta` (id + name +
        // raw fragment) as it arrives (#194), and the block still finalizes into
        // the assembled `ToolCall` on `content_block_stop`.
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
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        let e1 = handle_frame(
            "content_block_delta",
            Some(d1),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            e1,
            vec![LlmEvent::ToolCallDelta {
                id: "t1".into(),
                name: "greet".into(),
                delta: "{\"nm\":".into(),
            }]
        );
        let e2 = handle_frame(
            "content_block_delta",
            Some(d2),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            e2,
            vec![LlmEvent::ToolCallDelta {
                id: "t1".into(),
                name: "greet".into(),
                delta: "\"sam\"}".into(),
            }]
        );
        let stop = handle_frame(
            "content_block_stop",
            None,
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            stop,
            vec![LlmEvent::ToolCall(crate::ToolCall {
                id: "t1".into(),
                name: "greet".into(),
                input: r#"{"nm":"sam"}"#.into(),
                provider_meta: None,
            })]
        );
    }

    #[test]
    fn usage_is_captured_from_frames() {
        let mut usage = Usage::default();
        let mut stop = None;
        let _ = handle_frame(
            "message_start",
            Some(json!({ "message": { "usage": {
                "input_tokens": 42,
                "cache_read_input_tokens": 10,
                "cache_creation_input_tokens": 5
            } } })),
            &mut None,
            &mut usage,
            &mut stop,
        )
        .unwrap();
        let _ = handle_frame(
            "message_delta",
            Some(json!({ "delta": { "stop_reason": "max_tokens" }, "usage": { "output_tokens": 7 } })),
            &mut None,
            &mut usage,
            &mut stop,
        )
        .unwrap();
        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.cached_input_tokens, Some(10));
        assert_eq!(usage.cache_write_tokens, Some(5));
        assert_eq!(stop, Some(StopReason::MaxTokens));
    }

    #[test]
    fn parse_frame_reads_event_and_data() {
        let frame = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
        let (event, data) = parse_frame(frame);
        assert_eq!(event, "content_block_delta");
        assert_eq!(data.unwrap()["delta"]["text"], "x");
    }

    // ── provider-side web search (#305) ─────────────────────────────────────

    #[test]
    fn body_omits_web_search_server_tool_without_config() {
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            None,
            None,
        );
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn body_pushes_web_search_server_tool_when_configured() {
        let ws = WebSearchConfig {
            enabled: true,
            max_uses: Some(4),
            allowed_domains: vec!["docs.rs".into()],
        };
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            None,
            Some(&ws),
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "web_search_20250305");
        assert_eq!(tools[0]["name"], "web_search");
        assert_eq!(tools[0]["max_uses"], 4);
        assert_eq!(tools[0]["allowed_domains"][0], "docs.rs");
    }

    #[test]
    fn web_search_server_tool_omits_unset_knobs() {
        let ws = WebSearchConfig {
            enabled: true,
            max_uses: None,
            allowed_domains: vec![],
        };
        let body = build_body(
            "claude-sonnet-4-5",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            None,
            Some(&ws),
        );
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "web_search_20250305");
        assert!(tool.get("max_uses").is_none());
        assert!(tool.get("allowed_domains").is_none());
    }

    #[test]
    fn server_tool_use_sequence_yields_reasoning_not_tool_call() {
        // server_tool_use start → input_json_delta (the query) → stop must surface
        // the query as Reasoning, never a ToolCall, and stream no ToolCallDelta.
        let start = json!({
            "content_block": { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {} }
        });
        let d1 = json!({ "delta": { "type": "input_json_delta", "partial_json": "{\"query\":" } });
        let d2 =
            json!({ "delta": { "type": "input_json_delta", "partial_json": "\"rust async\"}" } });

        let mut tool = None;
        let e0 = handle_frame(
            "content_block_start",
            Some(start),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert!(e0.is_empty());
        let e1 = handle_frame(
            "content_block_delta",
            Some(d1),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        let e2 = handle_frame(
            "content_block_delta",
            Some(d2),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        // A server tool streams no client ToolCallDelta.
        assert!(e1.is_empty() && e2.is_empty(), "no deltas for server tool");
        let stop = handle_frame(
            "content_block_stop",
            None,
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            stop,
            vec![LlmEvent::Reasoning("[web_search] rust async".into())]
        );
        assert!(
            !stop.iter().any(|e| matches!(e, LlmEvent::ToolCall(_))),
            "server tool must never yield a ToolCall"
        );
    }

    #[test]
    fn web_search_tool_result_block_renders_sources() {
        let block = json!({
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_1",
                "content": [
                    { "type": "web_search_result", "title": "Rust async", "url": "https://docs.rs/async" },
                    { "type": "web_search_result", "title": "Tokio", "url": "https://tokio.rs" }
                ]
            }
        });
        let mut tool = None;
        let evs = handle_frame(
            "content_block_start",
            Some(block),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            evs,
            vec![
                LlmEvent::Reasoning("[web_search] Rust async — https://docs.rs/async".into()),
                LlmEvent::Reasoning("[web_search] Tokio — https://tokio.rs".into()),
            ]
        );
    }

    #[test]
    fn web_search_tool_result_error_renders_error_line() {
        let block = json!({
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_1",
                "content": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" }
            }
        });
        let mut tool = None;
        let evs = handle_frame(
            "content_block_start",
            Some(block),
            &mut tool,
            &mut Usage::default(),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            evs,
            vec![LlmEvent::Reasoning(
                "[web_search] error: max_uses_exceeded".into()
            )]
        );
    }
}
