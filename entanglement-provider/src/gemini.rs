//! Native Google Gemini streaming client — hand-rolled over `reqwest`, no SDK
//! crate. Implements [`crate::Llm`] by POSTing to `:streamGenerateContent?alt=sse`
//! and parsing the Server-Sent-Events stream into [`LlmEvent`]s (incremental text,
//! reasoning "thought" parts, assembled function calls, terminal usage).
//!
//! # Why a native client, not the OpenAI-compat endpoint
//! Gemini exposes an OpenAI-compatible surface, but it does **not** round-trip
//! `thoughtSignature` — the opaque token a 2.5 thinking model attaches to a
//! function call that must be echoed back verbatim on the next turn, else the API
//! 4xxs on replayed history (#309). This client stashes that signature into
//! [`ToolCall::provider_meta`] on the way out and restores it when rebuilding
//! `contents` from history, so multi-turn tool use with a thinking model stays
//! valid. Core never inspects the field (ADR-0064-style opaque round-trip).
//!
//! # Wire shape (`streamGenerateContent`, SSE)
//! Frames are `data: <json>\n\n`; each payload is a `GenerateContentResponse`
//! chunk. Per chunk we care about `candidates[0].content.parts[]` — a `text` part
//! (or a `thought: true` text part → reasoning), or a `functionCall` part
//! (assembled immediately; Gemini sends args whole, not streamed) — plus
//! `candidates[0].finishReason` and the terminal `usageMetadata`.

use crate::client::HttpClient;
use crate::{Llm, LlmEvent, LlmRequest, LlmStream, StopReason, ToolCall, Usage};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

/// Default Gemini generative-language base (the `models` collection root).
pub const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Key under which the opaque Gemini `thoughtSignature` is stashed in
/// [`ToolCall::provider_meta`], so restore reads back exactly what stream wrote.
pub(crate) const THOUGHT_SIGNATURE_KEY: &str = "gemini_thought_signature";

/// Streaming Gemini client. Cheap to clone (the HTTP client is `Arc`-shared
/// internally); build one per session via [`gemini_factory`].
#[derive(Clone)]
pub struct GeminiLlm {
    base_url: String,
    api_key: String,
    default_model: String,
    /// Catalog-provided per-minute budget for this endpoint (`None` = client
    /// default). Threaded into the per-endpoint rate limiter (#241).
    rpm: Option<u32>,
    http: HttpClient,
}

impl GeminiLlm {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        http: HttpClient,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_model: default_model.into(),
            rpm,
            http,
        }
    }
}

/// Build an [`LlmFactory`] wired to Gemini. Each session gets its own cloned
/// [`GeminiLlm`]. `rpm = None` uses the client's default rate-limit budget.
pub fn gemini_factory(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = GeminiLlm::new(base_url, api_key, default_model, rpm, http);
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for GeminiLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        let body = build_body(req.system, req.messages, req.tools, req.generation);
        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/{model}:streamGenerateContent?alt=sse");

        tracing::debug!(
            model = %model,
            messages_count = req.messages.len(),
            tools_count = req.tools.len(),
            "gemini request"
        );
        crate::client::log_request_body("gemini", &body);

        // The rate-limit / retry pool is keyed by (endpoint, api_key); use the
        // base (key-agnostic) so every model on this endpoint shares one bucket.
        let (response, guard) = self
            .http
            .execute_with_retry(base, Some(&self.api_key), self.rpm, || {
                self.http
                    .client()
                    .post(&url)
                    .header("x-goog-api-key", &self.api_key)
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
            })
            .await
            .map_err(|e| match e {
                crate::client::RetryError::Permanent(e) => {
                    anyhow::anyhow!("gemini request failed: {e}")
                }
                crate::client::RetryError::Exhausted(attempts, e) => {
                    anyhow::anyhow!("gemini request failed after {attempts} attempts: {e}")
                }
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = crate::client::extract_retry_after_from_response(&response);
            let text = response.text().await.unwrap_or_default();
            tracing::error!(status = %status, response = %text, "gemini request failed");
            if status.as_u16() == 429 {
                if let Some(retry_after) = retry_after {
                    tracing::warn!(retry_after = ?retry_after, "rate limited, backing off");
                    return Err(anyhow::anyhow!(
                        "gemini rate limited, retry after {retry_after:?}"
                    ));
                }
            }
            anyhow::bail!("gemini HTTP {status}: {text}");
        }

        // Forward the SSE body with a per-chunk idle-gap watchdog (#241).
        let rx = crate::client::spawn_byte_stream(response, "gemini", guard);

        let stream = try_stream! {
            let mut buf = String::new();
            let mut usage = Usage::default();
            let mut finish_reason: Option<String> = None;
            let mut saw_tool_call = false;
            let mut rx = rx;

            while let Some(item) = rx.recv().await {
                let chunk = item?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(idx) = buf.find("\n\n") {
                    let frame: String = buf.drain(..idx + 2).collect();
                    let Some(data) = parse_frame(&frame) else { continue };
                    for ev in handle_chunk(&data, &mut usage, &mut finish_reason)? {
                        if matches!(ev, LlmEvent::ToolCall(_)) {
                            saw_tool_call = true;
                        }
                        yield ev;
                    }
                }
            }

            // Gemini reports `STOP` even for a function-call turn; upgrade to
            // ToolUse when we actually emitted a call so the reason isn't lost.
            let stop_reason = match finish_reason.as_deref() {
                Some("STOP") if saw_tool_call => Some(StopReason::ToolUse),
                Some(r) => Some(StopReason::from_gemini(r)),
                None if saw_tool_call => Some(StopReason::ToolUse),
                None => None,
            };
            yield LlmEvent::Finish { stop_reason, usage };
        };

        tracing::debug!(model = %model, "gemini stream started");
        Ok(stream.boxed())
    }
}

// ── SSE frame parsing ─────────────────────────────────────────────────────────

/// Extract the JSON payload from one SSE frame (`data: <json>` lines, joined).
/// Returns `None` for a comment/keep-alive/blank frame or unparsable data.
fn parse_frame(frame: &str) -> Option<Value> {
    let mut data_parts: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_parts.push(rest.trim());
        }
    }
    if data_parts.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&data_parts.join("\n")).ok()
}

/// Map one parsed chunk to zero or more [`LlmEvent`]s, folding usage + the latest
/// `finishReason`. Pure (no I/O) so it unit-tests directly. A `functionCall` part
/// is assembled immediately — Gemini sends the whole arg object, not streamed —
/// and its `thoughtSignature` (if any) is stashed into `provider_meta` (#309).
fn handle_chunk(
    data: &Value,
    usage: &mut Usage,
    finish_reason: &mut Option<String>,
) -> Result<Vec<LlmEvent>, anyhow::Error> {
    let mut out = Vec::new();

    if let Some(parts) = data
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
    {
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                out.push(LlmEvent::ToolCall(function_call_to_tool_call(fc, part)));
            } else if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if text.is_empty() {
                    continue;
                }
                // A `thought: true` part is the model's extended reasoning.
                if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                    out.push(LlmEvent::Reasoning(text.to_string()));
                } else {
                    out.push(LlmEvent::Text(text.to_string()));
                }
            }
        }
    }

    if let Some(r) = data
        .pointer("/candidates/0/finishReason")
        .and_then(|v| v.as_str())
    {
        *finish_reason = Some(r.to_string());
    }

    if let Some(meta) = data.get("usageMetadata") {
        apply_usage(meta, usage);
    }

    Ok(out)
}

/// Build a [`ToolCall`] from a Gemini `functionCall` part. Gemini matches a
/// `functionResponse` back to its call by **name**, so the id is the name (the
/// runtime echoes `tool_call_id` as that name in [`convert_messages`]). The
/// `thoughtSignature` (a thinking model's opaque per-call token) is preserved in
/// `provider_meta` for verbatim round-trip on the next turn (#309).
fn function_call_to_tool_call(fc: &Value, part: &Value) -> ToolCall {
    let name = fc
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
    let input = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
    let provider_meta = part
        .get("thoughtSignature")
        .and_then(|v| v.as_str())
        .map(|sig| json!({ THOUGHT_SIGNATURE_KEY: sig }));
    ToolCall {
        id: name.clone(),
        name,
        input,
        provider_meta,
    }
}

/// Fold Gemini's `usageMetadata` into the normalized [`Usage`]. `promptTokenCount`
/// is the whole prompt including any cached read, so subtract the cached portion to
/// keep `input_tokens` uncached (no double-count against catalog pricing, #192).
fn apply_usage(meta: &Value, usage: &mut Usage) {
    let cached = meta.get("cachedContentTokenCount").and_then(|v| v.as_u64());
    if let Some(prompt) = meta.get("promptTokenCount").and_then(|v| v.as_u64()) {
        usage.input_tokens = Some(prompt.saturating_sub(cached.unwrap_or(0)));
    }
    if let Some(c) = cached {
        usage.cached_input_tokens = Some(c);
    }
    if let Some(out) = meta.get("candidatesTokenCount").and_then(|v| v.as_u64()) {
        usage.output_tokens = Some(out);
    }
}

mod request;
use request::build_body;

#[cfg(test)]
mod tests;
