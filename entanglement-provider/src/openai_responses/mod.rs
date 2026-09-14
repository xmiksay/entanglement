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
//! - **Native client-executed `tool_search`**: when any advertised
//!   [`crate::ToolSpec::defer_loading`] tool is present, the request declares
//!   `{"type":"tool_search","execution":"client"}` (our own schema, field
//!   `query`); a streamed `tool_search_call` output item is mapped onto the
//!   existing [`crate::ToolCall`]/[`crate::LlmEvent::ToolCall`] machinery
//!   under the reserved name [`TOOL_SEARCH_CALL_TOOL`] — **not** a new
//!   `LlmEvent` variant. That name is never a real registered tool; the
//!   runtime's dispatch ladder intercepts it exactly like the `explore`/
//!   `describe` pseudo-tools, runs the same search lookup, and replies with
//!   a [`crate::ContentPart::ToolSearchOutput`] block. This rides the
//!   *existing* `ToolExec`/`ToolResult` round-trip end to end — no
//!   `InMsg`/`OutEvent`/`protocol.rs` change was needed (see the module doc
//!   on the runtime's `discover::tool_search` for the dispatch side).
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
use crate::{Llm, LlmEvent, LlmRequest, LlmStream, ModelConcurrencyResolver, Usage};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use sse::{drain_available_frames, StreamState};

/// OpenAI Responses API base.
pub const OPENAI_RESPONSES_BASE: &str = "https://api.openai.com/v1";

/// Reserved [`crate::ToolCall::name`] the client emits for a streamed,
/// client-executed `tool_search_call` output item, and recognizes on replay
/// to emit a `tool_search_call`/`tool_search_output` input-item pair instead
/// of `function_call`/`function_call_output`. Never a real registered tool —
/// chosen distinct from any plausible user/MCP tool name, and from the
/// runtime's own `explore`/`describe` kernel tools (which stay independently
/// callable; the runtime answers a search call by running that same
/// explore+describe lookup, see `entanglement_runtime::discover::tool_search`).
pub const TOOL_SEARCH_CALL_TOOL: &str = "responses_tool_search";

/// Streaming OpenAI Responses API client. Cheap to clone (the HTTP client is
/// `Arc`-shared internally); build one per session via
/// [`openai_responses_factory`].
#[derive(Clone)]
pub struct OpenAiResponsesLlm {
    base_url: String,
    api_key: Option<String>,
    /// OAuth bearer source (mirrors [`crate::OpenAiLlm::auth`]) — `Some` wins
    /// over `api_key`; the pool identity stays `api_key` (ADR-0156).
    auth: Option<std::sync::Arc<dyn crate::oauth::AccessTokenSource>>,
    default_model: String,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    model_concurrency: ModelConcurrencyResolver,
    http: HttpClient,
}

impl OpenAiResponsesLlm {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        model_concurrency: ModelConcurrencyResolver,
        http: HttpClient,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            auth: None,
            default_model: default_model.into(),
            rpm,
            concurrency,
            model_concurrency,
            http,
        }
    }

    /// Authenticate with an OAuth bearer from `auth` instead of a static key
    /// — see [`crate::OpenAiLlm::with_auth`]'s field docs for the pool-identity rule.
    pub fn with_auth(mut self, auth: std::sync::Arc<dyn crate::oauth::AccessTokenSource>) -> Self {
        self.auth = Some(auth);
        self
    }
}

/// Factory for one per-session [`OpenAiResponsesLlm`]. Mirrors
/// [`crate::openai_factory`]'s parameter shape, minus the OpenAI-compat-only
/// knobs this wire doesn't (yet) carry: provider-side web search and the
/// `prompt_cache_key` hint (neither is part of the P7 scope — see the module
/// doc; both can be added later without touching the reserved-name contract
/// above).
#[allow(clippy::too_many_arguments)]
pub fn openai_responses_factory(
    base_url: impl Into<String>,
    api_key: Option<String>,
    auth: Option<std::sync::Arc<dyn crate::oauth::AccessTokenSource>>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    model_concurrency: ModelConcurrencyResolver,
    http: HttpClient,
) -> crate::LlmFactory {
    let mut llm = OpenAiResponsesLlm::new(
        base_url,
        api_key,
        default_model,
        rpm,
        concurrency,
        model_concurrency,
        http,
    );
    if let Some(auth) = auth {
        llm = llm.with_auth(auth);
    }
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for OpenAiResponsesLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        let model_concurrency = (self.model_concurrency)(&model);
        let body = request::build_body(&model, req.system, req.messages, req.tools, req.generation);
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        tracing::debug!(
            model = %model,
            base = %self.base_url,
            messages_count = req.messages.len(),
            tools_count = req.tools.len(),
            "openai-responses request"
        );
        crate::client::log_request_body("openai_responses", &body);

        let mut forced_refresh = false;
        let (response, guard) = loop {
            let bearer = match &self.auth {
                Some(source) => Some(source.access_token(forced_refresh).await.map_err(|e| {
                    anyhow::anyhow!("fetching the OAuth token for `{}`: {e:#}", self.base_url)
                })?),
                None => self.api_key.clone(),
            };
            let (response, guard) = self
                .http
                .execute_with_retry(
                    &self.base_url,
                    self.api_key.as_deref(),
                    self.rpm,
                    self.concurrency,
                    &model,
                    model_concurrency,
                    req.retry,
                    || {
                        let mut request = self.http.client().post(&url);
                        if let Some(key) = &bearer {
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
            if response.status().as_u16() == 401 && self.auth.is_some() && !forced_refresh {
                tracing::warn!(
                    base = %self.base_url,
                    "openai-responses 401 with an OAuth bearer; forcing one refresh and retrying"
                );
                forced_refresh = true;
                continue;
            }
            break (response, guard);
        };

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
