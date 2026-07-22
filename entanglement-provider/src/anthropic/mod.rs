//! Anthropic Messages API streaming client — hand-rolled over `reqwest`, no
//! Anthropic SDK crate. Implements [`crate::Llm`] by POSTing to
//! `/v1/messages` with `stream: true` and parsing the Server-Sent-Events stream
//! into [`LlmEvent`]s (incremental text, assembled tool calls, terminal usage).
//!
//! Split across three files (#481, to stay under the project's file-size cap):
//! this module owns the client + the streaming/continuation loop; [`request`]
//! owns request-body construction (`Message` history → Anthropic wire); [`sse`]
//! owns SSE frame parsing.
//!
//! # `pause_turn` continuation (#481, follow-up to #305/ADR-0075)
//! A long-running server-side tool (a web search) can end a response with
//! `stop_reason: "pause_turn"` instead of a confident stop — Anthropic's
//! contract is to resend the paused turn's content blocks verbatim as the next
//! request's trailing assistant message and let the model continue. This client
//! owns that loop entirely: [`sse::handle_frame`] accumulates every finalized
//! content block into a raw JSON array as the stream runs, and on `pause_turn`
//! `stream()` re-POSTs with that array appended as a fresh assistant turn,
//! continuing to yield events from the new stream with no `Finish` in between —
//! core never observes `pause_turn`, only the eventual confident stop. Bounded
//! by [`MAX_PAUSE_CONTINUATIONS`] so a pathologically repeating pause can't loop
//! forever; if the budget runs out, the client's own `Finish` still reports
//! `pause_turn` (mapped to [`StopReason::Other`]), and the turn loop's
//! ADR-0118 ambiguous-stop retry is the fallback safety net.
//!
//! The `Llm` trait + its DTOs live in this crate (the leaf); `entanglement-core`
//! depends on it and drives `dyn Llm` from the engine loop (ADR-0053, which
//! inverted the original trait-in-core seam of ADR-0006 / ADR-0007).

mod request;
mod sse;

use crate::client::HttpClient;
use crate::web_search::WebSearchConfig;
use crate::{Llm, LlmEvent, LlmRequest, LlmStream, StopReason, Usage};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use sse::{handle_frame, parse_frame, PendingTool};

pub(crate) use request::coalesce_same_role;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Fallback output cap when the request carries no
/// [`GenerationParams::max_output_tokens`] (Anthropic *requires* `max_tokens`).
const DEFAULT_MAX_TOKENS: u32 = 16_384;
/// Bound on how many times `stream()` re-POSTs after a `pause_turn` before
/// giving up and surfacing it as the terminal stop reason (#481). Generous —
/// a real long-running search sequence is expected to pause a handful of
/// times at most — while still guaranteeing termination.
const MAX_PAUSE_CONTINUATIONS: usize = 6;

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
    /// the web-search server tool. Bound at construction, invisible to core.
    web_search: Option<WebSearchConfig>,
    /// Server-tool type string to request when `web_search` is `Some` (#481) —
    /// the resolved `ModelEntry::web_search_tool_version` capability flag for
    /// the bound model. `None` falls back to the client's own `_20250305`
    /// default (see `request::web_search_tool_entry`).
    web_search_tool_version: Option<String>,
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
        web_search_tool_version: Option<String>,
        http: HttpClient,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            default_model: default_model.into(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
            rpm,
            concurrency,
            web_search,
            web_search_tool_version,
            http,
        }
    }
}

/// Build an [`LlmFactory`] wired to Anthropic. Each session gets its own cloned
/// [`AnthropicLlm`]. `rpm`/`concurrency = None` use the client's defaults;
/// `web_search = Some(..)` requests provider-side web search (#305);
/// `web_search_tool_version` selects the server-tool type when set (#481).
#[allow(clippy::too_many_arguments)]
pub fn anthropic_factory(
    api_key: impl Into<String>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    web_search: Option<WebSearchConfig>,
    web_search_tool_version: Option<String>,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = AnthropicLlm::new(
        api_key,
        default_model,
        rpm,
        concurrency,
        web_search,
        web_search_tool_version,
        http,
    );
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

/// Consume `response`, returning it unchanged on a success status or an `Err`
/// (after draining the body for the error text) otherwise. Factored out of
/// `stream()`'s `try_stream!` loop as a plain `async fn` so the borrow checker
/// sees an unambiguous move (a bare `?` on a value partially consumed by
/// `.text()` only on *some* paths confused NLL's reinitialization analysis
/// when inlined directly inside the loop, #481).
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
            self.web_search.as_ref(),
            self.web_search_tool_version.as_deref(),
        );
        // The original conversation's wire `messages` — the base a `pause_turn`
        // continuation replays from, plus its own accumulated trailing turn.
        let base_messages: Vec<Value> = body["messages"].as_array().cloned().unwrap_or_default();

        tracing::debug!(
            model = %model,
            messages_count = req.messages.len(),
            tools_count = req.tools.len(),
            "anthropic request"
        );
        crate::client::log_request_body("anthropic", &body);

        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let rpm = self.rpm;
        let concurrency = self.concurrency;
        let mut body = body;

        let stream = try_stream! {
            let mut cumulative_usage = Usage::default();
            let mut assembled_blocks: Vec<Value> = Vec::new();
            let mut continuations: usize = 0;

            loop {
                let (response, guard) = http
                    .execute_with_retry(
                        ANTHROPIC_API_URL,
                        Some(&api_key),
                        rpm,
                        concurrency,
                        || {
                            http.client()
                                .post(ANTHROPIC_API_URL)
                                .header("x-api-key", &api_key)
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
                            anyhow::anyhow!("anthropic request failed after {attempts} attempts: {e}")
                        }
                    })?;

                let response = ensure_success(response).await?;

                // Forward the SSE body with a per-chunk idle-gap watchdog (#241): a
                // long healthy stream runs to completion, a hung one dies within
                // the gap.
                let rx = crate::client::spawn_byte_stream(response, "anthropic", guard);

                // Byte-buffered framing (#443): a multi-byte UTF-8 character can
                // straddle two network chunks, so decoding must wait for a
                // complete `\n\n`-terminated frame — see `sse_frame::SseFrameBuffer`.
                let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n\n");
                let mut current_tool: Option<PendingTool> = None;
                let mut current_text: Option<String> = None;
                let mut usage = Usage::default();
                let mut stop_reason: Option<StopReason> = None;
                let mut pause_turn = false;
                let mut rx = rx;

                while let Some(item) = rx.recv().await {
                    let chunk = item?;
                    frames.push(&chunk);
                    while let Some(frame_owned) = frames.next_frame() {
                        let (event, data) = parse_frame(&frame_owned);
                        for ev in handle_frame(
                            &event,
                            data,
                            &mut current_tool,
                            &mut current_text,
                            &mut assembled_blocks,
                            &mut usage,
                            &mut stop_reason,
                            &mut pause_turn,
                        )? {
                            yield ev;
                        }
                    }
                }

                cumulative_usage.input_tokens = Some(
                    cumulative_usage.input_tokens.unwrap_or(0) + usage.input_tokens.unwrap_or(0),
                );
                cumulative_usage.output_tokens = Some(
                    cumulative_usage.output_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0),
                );
                cumulative_usage.cached_input_tokens = Some(
                    cumulative_usage.cached_input_tokens.unwrap_or(0)
                        + usage.cached_input_tokens.unwrap_or(0),
                );
                cumulative_usage.cache_write_tokens = Some(
                    cumulative_usage.cache_write_tokens.unwrap_or(0)
                        + usage.cache_write_tokens.unwrap_or(0),
                );

                if pause_turn && continuations < MAX_PAUSE_CONTINUATIONS && !assembled_blocks.is_empty() {
                    continuations += 1;
                    tracing::debug!(continuations, "anthropic pause_turn - continuing in place");
                    let mut wire_messages = base_messages.clone();
                    wire_messages.push(json!({
                        "role": "assistant",
                        "content": Value::Array(assembled_blocks.clone()),
                    }));
                    body["messages"] = Value::Array(wire_messages);
                    continue;
                }

                yield LlmEvent::Finish { stop_reason, usage: cumulative_usage };
                break;
            }
        };

        tracing::debug!(model = %model, "anthropic stream started");
        Ok(stream.boxed())
    }
}
