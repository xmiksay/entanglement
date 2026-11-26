//! OpenAI's native **Responses API** client (`/responses`), distinct from the
//! Chat Completions wire [`crate::openai::OpenAiLlm`] speaks. Opt-in per
//! catalog entry (`wire: openai_responses`, ADR-0196 §3, P7) — `openai`
//! itself stays on Chat Completions; nothing flips the default.
//!
//! Two things make this wire worth a second client instead of a branch in the
//! existing one:
//!
//! - **Flat typed input/output items**, not role+content messages: a
//!   `function_call`/`function_call_output` pair correlates by `call_id`
//!   instead of Chat Completions' single `tool_calls` array +
//!   `role: "tool"` message. See [`request`].
//!
//! Stateless: like every other client here, history replay resends the whole
//! `input` array every request (`previous_response_id` — OpenAI's optional
//! server-side state — is never used); the engine owns history, not OpenAI.
//!
//! Split across three files (mirroring `openai/`, #481's file-cap pattern):
//! this module owns the client + streaming loop; [`request`] owns request-body
//! construction; [`sse`] owns SSE event parsing.

mod request;
mod sse;
#[cfg(test)]
mod tests;

use crate::client::HttpClient;
use crate::{LimitsResolver, Llm, LlmEvent, LlmRequest, LlmStream, Usage};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use sse::{drain_available_frames, StreamState};

/// OpenAI Responses API base.
pub const OPENAI_RESPONSES_BASE: &str = "https://api.openai.com/v1";

/// Streaming OpenAI Responses API client. Cheap to clone (the HTTP client is
/// `Arc`-shared internally); build one per session via
/// [`openai_responses_factory`].
#[derive(Clone)]
pub struct OpenAiResponsesLlm {
    base_url: String,
    api_key: Option<String>,
    default_model: String,
    limits: LimitsResolver,
    /// Resolves the request model's thinking facts per request (#550 shape,
    /// mirroring [`crate::OpenAiLlm`]) — here only the effort clamp is used.
    thinking_spec: crate::ThinkingSpecResolver,
    http: HttpClient,
}

impl OpenAiResponsesLlm {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: impl Into<String>,
        limits: LimitsResolver,
        thinking_spec: crate::ThinkingSpecResolver,
        http: HttpClient,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            default_model: default_model.into(),
            limits,
            thinking_spec,
            http,
        }
    }
}

/// Factory for one per-session [`OpenAiResponsesLlm`]. Mirrors
/// [`crate::openai_factory`]'s parameter shape, minus the OpenAI-compat-only
/// knobs this wire doesn't (yet) carry: provider-side web search and the
/// `prompt_cache_key` hint (neither is part of the P7 scope — see the module
/// doc; both can be added later without touching the reserved-name contract
/// above). `thinking_spec` resolves the request model's effort tiers
/// (`Catalog::thinking_spec_resolver`), clamping `reasoning.effort` per request.
pub fn openai_responses_factory(
    base_url: impl Into<String>,
    api_key: Option<String>,
    default_model: impl Into<String>,
    limits: LimitsResolver,
    thinking_spec: crate::ThinkingSpecResolver,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = OpenAiResponsesLlm::new(
        base_url,
        api_key,
        default_model,
        limits,
        thinking_spec,
        http,
    );
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for OpenAiResponsesLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        let limits = (self.limits)(&model);
        let thinking = (self.thinking_spec)(&model);
        let body = request::build_body(
            &model,
            req.system,
            req.messages,
            req.tools,
            req.generation,
            thinking,
        );
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        tracing::debug!(
            model = %model,
            base = %self.base_url,
            messages_count = req.messages.len(),
            tools_count = req.tools.len(),
            "openai-responses request"
        );
        crate::client::log_request_body("openai_responses", &body);

        let (response, guard) = self
            .http
            .execute_with_retry(
                &self.base_url,
                self.api_key.as_deref(),
                &model,
                limits,
                limits.estimate_tokens(&body),
                req.retry,
                || {
                    let mut request = self.http.client().post(&url);
                    if let Some(key) = &self.api_key {
                        request = request.bearer_auth(key);
                    }
                    request.json(&body).send()
                },
            )
            .await
            .map_err(|e| match e {
                crate::client::RetryError::Permanent(e) => {
                    anyhow::anyhow!("openai-responses request failed: {e}")
                }
                crate::client::RetryError::Exhausted(attempts, e) => anyhow::anyhow!(
                    "openai-responses request failed after {} attempts: {e}",
                    attempts
                ),
                crate::client::RetryError::RateLimited => anyhow::anyhow!(
                    "openai-responses rate limited: gave up waiting for the endpoint to clear"
                ),
                crate::client::RetryError::HeaderTimeout(timeout, attempts) => anyhow::anyhow!(
                    "openai-responses request failed: no response headers within {timeout:?} \
                     after {attempts} attempt(s)"
                ),
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = crate::client::extract_retry_after_from_response(&response);
            let text = response.text().await.unwrap_or_default();
            tracing::error!(status = %status, response = %text, "openai-responses request failed");
            if status.as_u16() == 429 {
                if let Some(retry_after) = retry_after {
                    tracing::warn!(retry_after = ?retry_after, "rate limited, backing off");
                    return Err(anyhow::anyhow!(
                        "openai-responses rate limited, retry after {:?}",
                        retry_after
                    ));
                }
            }
            anyhow::bail!("openai-responses HTTP {status}: {text}");
        }

        let rx = crate::client::spawn_byte_stream(response, "openai-responses", guard);

        let stream = try_stream! {
            let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n");
            let mut state = StreamState::default();
            let mut usage = Usage::default();
            let mut rx = rx;

            'outer: while let Some(item) = rx.recv().await {
                let chunk = item?;
                frames.push(&chunk);
                let (events, done) = drain_available_frames(&mut frames, &mut state, &mut usage)?;
                for ev in events {
                    yield ev;
                }
                if done {
                    break 'outer;
                }
            }
            // A stream cut before `response.completed`/`response.failed`
            // arrived: report whatever tool calls were assembled as an
            // ambiguous stop (no confident `StopReason`) rather than
            // silently ending the turn, mirroring the OpenAI-compat client's
            // EOF-without-finish-reason handling (ADR-0118).
            if !state.terminated {
                let stop_reason = if state.emitted_any_tool_call {
                    Some(crate::StopReason::ToolUse)
                } else {
                    None
                };
                yield LlmEvent::Finish { stop_reason, usage };
            }
        };

        tracing::debug!(model = %model, base = %self.base_url, "openai-responses stream started");
        Ok(stream.boxed())
    }
}
