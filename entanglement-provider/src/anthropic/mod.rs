//! Anthropic Messages API streaming client — hand-rolled over `reqwest`, no
//! Anthropic SDK crate. Implements [`crate::Llm`] by POSTing to
//! `/v1/messages` with `stream: true` and parsing the Server-Sent-Events stream
//! into [`LlmEvent`]s (incremental text, assembled tool calls, terminal usage).
//!
//! Split across three files to stay under the project's file-size cap: this
//! module owns the client + the streaming loop; [`request`] owns request-body
//! construction (`Message` history → Anthropic wire); [`sse`] owns SSE frame
//! parsing.
//!
//! The `Llm` trait + its DTOs live in this crate (the leaf); `entanglement-core`
//! depends on it and drives `dyn Llm` from the engine loop (ADR-0053, which
//! inverted the original trait-in-core seam of ADR-0006 / ADR-0007).

mod request;
mod sse;

use crate::client::HttpClient;
use crate::{
    AnthropicModelSpec, LimitsResolver, Llm, LlmEvent, LlmRequest, LlmStream, StopReason, Usage,
};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use sse::{handle_frame, parse_frame, PendingTool};

pub(crate) use request::{append_final_user_block, coalesce_same_role};

/// Default base — no path — mirroring [`crate::openai::OPENAI_BASE`]/
/// [`crate::gemini::GEMINI_BASE`]. A catalog `base_url` (a proxy/gateway
/// speaking the Anthropic wire) overrides this per #551; before that fix the
/// URL was hard-coded and `base_url` silently ignored.
pub const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Fallback output cap when the request carries no
/// [`GenerationParams::max_output_tokens`] (Anthropic *requires* `max_tokens`).
const DEFAULT_MAX_TOKENS: u32 = 16_384;

/// Streaming Anthropic Messages client. Cheap to clone (the HTTP client is
/// `Arc`-shared internally); build one per session via [`anthropic_factory`].
#[derive(Clone)]
pub struct AnthropicLlm {
    api_key: String,
    /// Base URL, no trailing slash required — `/v1/messages` is appended per
    /// request. Defaults to [`ANTHROPIC_BASE`]; a catalog `base_url` (a
    /// proxy/gateway) overrides it (#551).
    base_url: String,
    default_model: String,
    /// Fallback output cap ([`DEFAULT_MAX_TOKENS`]) used only when a request omits
    /// its own [`GenerationParams::max_output_tokens`] (#191).
    default_max_tokens: u32,
    /// Resolves the endpoint's and the requested model's rate limits, per
    /// request (#550).
    limits: LimitsResolver,
    /// The bound model's wire facts — thinking shape, replay, effort tiers,
    /// whether it takes `temperature` — resolved once at construction
    /// (`Catalog::anthropic_model_spec`). Anthropic's fixed-budget and adaptive
    /// forms are mutually exclusive and the newer models 400 on `budget_tokens`
    /// and on any sampling parameter, so these are per-model catalog facts
    /// rather than client constants. See [`AnthropicModelSpec`] for the
    /// `model:`-only-pin caveat of binding them at construction.
    model_spec: AnthropicModelSpec,
    http: HttpClient,
}

impl AnthropicLlm {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        limits: LimitsResolver,
        model_spec: AnthropicModelSpec,
        http: HttpClient,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            default_model: default_model.into(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
            limits,
            model_spec,
            http,
        }
    }
}

/// Build an [`LlmFactory`] wired to Anthropic. Each session gets its own cloned
/// [`AnthropicLlm`]. `base_url` overrides [`ANTHROPIC_BASE`] — a proxy/gateway
/// catalog entry (#551); `limits` resolves the endpoint's and the request
/// model's rate limits per request (#550); `model_spec` carries the bound
/// model's thinking shape, replay flag, effort tiers and temperature support
/// (`Catalog::anthropic_model_spec`, resolved once for `default_model`).
pub fn anthropic_factory(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    default_model: impl Into<String>,
    limits: LimitsResolver,
    model_spec: AnthropicModelSpec,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = AnthropicLlm::new(base_url, api_key, default_model, limits, model_spec, http);
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

/// Consume `response`, returning it unchanged on a success status or an `Err`
/// (after draining the body for the error text) otherwise.
async fn ensure_success(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let retry_after = crate::client::extract_retry_after_from_response(&response);
    let text = response.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        if let Some(retry_after) = retry_after {
            tracing::warn!(retry_after = ?retry_after, "rate limited, backing off");
            anyhow::bail!("anthropic rate limited, retry after {:?}", retry_after);
        }
    }
    anyhow::bail!("anthropic HTTP {status}: {text}");
}

#[async_trait]
impl Llm for AnthropicLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        let body = request::build_body(
            &model,
            req.system,
            req.messages,
            req.tools,
            self.default_max_tokens,
            req.generation,
            self.model_spec,
            req.trailing_notice.as_deref(),
        );

        tracing::debug!(
            model = %model,
            messages_count = req.messages.len(),
            tools_count = req.tools.len(),
            "anthropic request"
        );
        crate::client::log_request_body("anthropic", &body);

        let http = self.http.clone();
        let api_key = self.api_key.clone();
        // Base defaults to `ANTHROPIC_BASE`; a catalog `base_url` (a
        // proxy/gateway speaking the Anthropic wire) overrides it (#551) — the
        // trim mirrors `openai`/`gemini`'s own request-URL construction.
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        // Aux fail-fast override (#560 follow-up): `Copy`, so extracted here
        // — `req`'s borrow can't cross into the `'static` `try_stream!`
        // generator below.
        let retry = req.retry;
        // Resolved against *this* request's model, not baked in at
        // construction (#550) — a profile's `model:`-only pin can send a
        // request under a different model than `default_model`.
        let limits = (self.limits)(&model);
        // A separate clone for the stream body below: `model` itself is still
        // read after the block builds (the "stream started" trace), and
        // `try_stream!` captures by move.
        let request_model = model.clone();

        let stream = try_stream! {
            let (response, guard) = http
                .execute_with_retry(
                    &url,
                    Some(api_key.as_str()),
                    &request_model,
                    limits,
                    limits.estimate_tokens(&body),
                    retry,
                    || {
                        http.client()
                            .post(&url)
                            .header("anthropic-version", ANTHROPIC_VERSION)
                            .header("x-api-key", &api_key)
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
                        anyhow::anyhow!("anthropic request failed after {attempts} attempts: {e}")
                    }
                    crate::client::RetryError::RateLimited => {
                        anyhow::anyhow!("anthropic rate limited: gave up waiting for the endpoint to clear")
                    }
                    crate::client::RetryError::HeaderTimeout(timeout, attempts) => {
                        anyhow::anyhow!(
                            "anthropic request failed: no response headers within \
                             {timeout:?} after {attempts} attempt(s)"
                        )
                    }
                })?;

            let response = ensure_success(response).await?;

            // Forward the SSE body with a per-chunk idle-gap watchdog (#241): a
            // long healthy stream runs to completion, a hung one dies within
            // the gap.
            let mut rx = crate::client::spawn_byte_stream(response, "anthropic", guard);

            // Byte-buffered framing (#443): a multi-byte UTF-8 character can
            // straddle two network chunks, so decoding must wait for a
            // complete `\n\n`-terminated frame — see `sse_frame::SseFrameBuffer`.
            let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n\n");
            let mut current_tool: Option<PendingTool> = None;
            let mut current_thinking: Option<sse::PendingThinking> = None;
            let mut usage = Usage::default();
            let mut stop_reason: Option<StopReason> = None;

            while let Some(item) = rx.recv().await {
                let chunk = item?;
                frames.push(&chunk);
                while let Some(frame_owned) = frames.next_frame() {
                    let (event, data) = parse_frame(&frame_owned);
                    for ev in handle_frame(
                        &event,
                        data,
                        &mut current_tool,
                        &mut current_thinking,
                        &mut usage,
                        &mut stop_reason,
                    )? {
                        yield ev;
                    }
                }
            }

            yield LlmEvent::Finish { stop_reason, usage };
        };

        tracing::debug!(model = %model, "anthropic stream started");
        Ok(stream.boxed())
    }
}
