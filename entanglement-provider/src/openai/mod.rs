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
//! Split across three files (#481, to stay under the project's file-size cap):
//! this module owns the client + streaming loop; [`request`] owns request-body
//! construction (`Message` history → OpenAI wire); [`sse`] owns SSE chunk
//! parsing.
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
//!   (the model wants to run tools; every assembled tool flushes as a
//!   validated [`LlmEvent::ToolCall`] once the stream ends — see
//!   `sse::flush_pending_tools`, #445)
//! - `usage` (final chunk)             → token counts
//!
//! Tool-result messages round-trip as `role: "tool"` **per call** — unlike
//! Anthropic, which merges consecutive results into one user turn (that's why
//! Anthropic keeps its own module). See ADR-0007 for why backends live outside
//! `entanglement-core`.

mod request;
mod sse;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use crate::client::HttpClient;
use crate::web_search::WebSearchConfig;
use crate::{Llm, LlmEvent, LlmRequest, LlmStream, MessageRole, StopReason, ToolCall, Usage};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use sse::{
    drain_available_frames, flush_pending_tools, handle_chunk, note_finish_reason, parse_sse_line,
    SseEvent,
};

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
    /// Catalog-provided in-flight concurrency cap for this endpoint (`None` =
    /// client default). Threaded into the per-endpoint concurrency permit (#414).
    concurrency: Option<usize>,
    /// Opt-in provider-side web search (#305): when `Some`, `build_body` requests
    /// the z.ai `web_search` tool. Bound at construction, invisible to core.
    web_search: Option<WebSearchConfig>,
    http: HttpClient,
}

impl OpenAiLlm {
    /// `api_key = None` sends no `Authorization` header (Ollama). A `Some` key is
    /// sent as `Bearer`. `rpm`/`concurrency = None` use the client's defaults.
    /// `web_search = Some(..)` requests provider-side web search (#305).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        web_search: Option<WebSearchConfig>,
        http: HttpClient,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            default_model: default_model.into(),
            rpm,
            concurrency,
            web_search,
            http,
        }
    }
}

/// Factory for one per-session [`OpenAiLlm`]. Pass the provider's base URL, an
/// optional key, the default model id, the endpoint's rpm budget, the endpoint's
/// concurrency cap (#414), and the opt-in [`WebSearchConfig`] (`None` disables
/// provider-side web search, #305).
#[allow(clippy::too_many_arguments)]
pub fn openai_factory(
    base_url: impl Into<String>,
    api_key: Option<String>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    web_search: Option<WebSearchConfig>,
    http: HttpClient,
) -> crate::LlmFactory {
    let llm = OpenAiLlm::new(
        base_url,
        api_key,
        default_model,
        rpm,
        concurrency,
        web_search,
        http,
    );
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for OpenAiLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        let body = request::build_body(
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

        let (response, guard) = self
            .http
            .execute_with_retry(
                &self.base_url,
                self.api_key.as_deref(),
                self.rpm,
                self.concurrency,
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
        let rx = crate::client::spawn_byte_stream(response, "openai-compat", guard);

        let stream = try_stream! {
            // Byte-buffered framing (#443): a multi-byte UTF-8 character can
            // straddle two network chunks, so decoding must wait for a complete
            // `\n`-terminated frame — see `sse_frame::SseFrameBuffer`.
            let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n");
            let mut tools: BTreeMap<u32, PendingTool> = BTreeMap::new();
            let mut usage = Usage::default();
            let mut seen_finish_reason: Option<String> = None;
            let mut rx = rx;
            let mut saw_done = false;

            'outer: while let Some(item) = rx.recv().await {
                let chunk = item?;
                frames.push(&chunk);
                let (events, done) =
                    drain_available_frames(&mut frames, &mut tools, &mut usage, &mut seen_finish_reason)?;
                for ev in events {
                    yield ev;
                }
                if done {
                    // Protocol-correct terminator (#483): stop reading immediately,
                    // ignoring anything the endpoint sends afterward instead of
                    // relying on the connection to close.
                    saw_done = true;
                    break 'outer;
                }
            }
            // Flush a final unterminated frame at EOF (#483): a stream cut mid-frame,
            // or a server that omits the trailing delimiter on its last event, would
            // otherwise silently drop the closing chunk — which can carry
            // `finish_reason`, the difference between a confident stop and an
            // ambiguous-stop retry (ADR-0118). Skipped once `[DONE]` was already
            // seen: whatever is still buffered after it is protocol garbage, not an
            // event to parse.
            if !saw_done {
                if let Some(trailing) = frames.take_remaining() {
                    if let SseEvent::Data(data) = parse_sse_line(&trailing) {
                        note_finish_reason(&data, &mut seen_finish_reason);
                        for ev in handle_chunk(&data, &mut tools, &mut usage)? {
                            yield ev;
                        }
                    }
                }
            }
            // Post-#445 `handle_chunk` no longer flushes eagerly, so a non-empty
            // `tools` map at stream end is the **normal** tool-use path, not an
            // anomaly — kept only to detect genuine data loss after the flush.
            let had_pending_tools = !tools.is_empty();
            // Single validating flush site (#445) for both the explicit
            // `finish_reason == "tool_calls"` case and the no-finish-reason
            // fallback — `tools` still holds everything assembled across the
            // whole stream here.
            let mut flushed = Vec::new();
            let emitted_any_tool_call = flush_pending_tools(&mut tools, &mut flushed);
            for ev in flushed {
                yield ev;
            }
            // Warn only on real data loss: tool calls were assembled but *every*
            // one had malformed/incomplete JSON arguments (e.g. a stream
            // truncated mid-arguments), so nothing valid was emitted. The normal
            // path — valid calls flushed just above — stays silent (this used to
            // warn on every tool-use turn, since #445 defers the flush to here).
            if had_pending_tools && !emitted_any_tool_call {
                tracing::warn!(
                    finish_reason = seen_finish_reason.as_deref().unwrap_or("none"),
                    "stream ended with tool calls whose arguments were incomplete - dropped (likely a truncated stream)"
                );
            }
            // A tool-flush with no valid call actually emitted (every pending
            // tool had malformed JSON args, or the stream ended without one)
            // must not report a confident-looking `ToolUse` stop with zero
            // `LlmEvent::ToolCall`s behind it — that contradiction left the
            // turn loop unable to tell a genuine tool-use stop from an
            // ambiguous one (ADR-0118). This applies equally whether
            // `finish_reason` was the explicit `"tool_calls"` or absent.
            let stop_reason = match seen_finish_reason.as_deref() {
                Some("tool_calls") if !emitted_any_tool_call => None,
                Some(r) => Some(StopReason::from_openai(r)),
                None if emitted_any_tool_call => Some(StopReason::ToolUse),
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
