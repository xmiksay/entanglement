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
//! Split to stay under the project's file-size cap (#481, #745): this module
//! owns the client + transport; [`request`] owns request-body construction
//! (`Message` history → OpenAI wire); [`stream`] decodes the response into
//! events; [`sse`] owns SSE chunk parsing; [`think`] splits inline think tags.
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
mod stream;
#[cfg(test)]
mod tests;
mod think;

use crate::client::HttpClient;
use crate::web_search::WebSearchConfig;
use crate::{Llm, LlmRequest, LlmStream, MessageRole, ModelConcurrencyResolver};
use async_trait::async_trait;
use sse::PendingTool;
use think::ThinkSplitter;

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
    /// OAuth bearer source (#684 edge d): `Some` wins over `api_key` — the
    /// token is fetched per request (cached until expiry by the source) and
    /// sent as `Authorization: Bearer`, with one forced-refresh retry on a
    /// `401`. The endpoint-pool identity stays `api_key` (usually `None` for
    /// an OAuth endpoint): a rotating bearer must never key the pool, or every
    /// refresh would mint a fresh rate-limit bucket (ADR-0156).
    auth: Option<std::sync::Arc<dyn crate::oauth::AccessTokenSource>>,
    default_model: String,
    /// Catalog-provided per-minute budget for this endpoint (`None` = client
    /// default). Threaded into the per-endpoint rate limiter (#241).
    rpm: Option<u32>,
    /// Catalog-provided in-flight concurrency cap for this endpoint (`None` =
    /// client default). Threaded into the per-endpoint concurrency permit (#414).
    concurrency: Option<usize>,
    /// Resolves the in-flight concurrency cap for whichever model a given
    /// request actually names, re-run per request rather than baked in at
    /// construction (#521/#550, ADR-0140) — threaded into the per-model
    /// concurrency permit layered under the endpoint permit.
    model_concurrency: ModelConcurrencyResolver,
    /// Opt-in provider-side web search (#305): when `Some`, `build_body` requests
    /// the z.ai `web_search` tool. Bound at construction, invisible to core.
    web_search: Option<WebSearchConfig>,
    /// Whether to forward [`LlmRequest::cache_key`] as OpenAI's
    /// `prompt_cache_key` implicit-cache routing hint (#673). Catalog-gated
    /// per provider (`ProviderEntry::prompt_cache_key`) — off for endpoints
    /// not verified to tolerate the extra body field.
    send_prompt_cache_key: bool,
    /// Resolves the thinking handling ([`crate::ThinkingSpec`], ADR-0191) for
    /// whichever model a given request actually names, re-run per request
    /// rather than baked in at construction (#550) — an inline-tags model's
    /// `<think>` spans must be split from its spoken text even when the client
    /// was constructed for a different model.
    thinking_spec: crate::ThinkingSpecResolver,
    http: HttpClient,
}

impl OpenAiLlm {
    /// `api_key = None` sends no `Authorization` header (Ollama). A `Some` key is
    /// sent as `Bearer`. `rpm`/`concurrency = None` use the client's (or the
    /// endpoint's) defaults; `model_concurrency` is consulted per request (#550).
    /// `web_search = Some(..)` requests provider-side web search (#305).
    /// `thinking_spec` resolves the model's thinking handling per request
    /// (ADR-0191); `fixed_thinking_spec(ThinkingSpec::default())` keeps the
    /// pre-ADR-0191 behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        model_concurrency: ModelConcurrencyResolver,
        web_search: Option<WebSearchConfig>,
        send_prompt_cache_key: bool,
        thinking_spec: crate::ThinkingSpecResolver,
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
            web_search,
            send_prompt_cache_key,
            thinking_spec,
            http,
        }
    }

    /// Authenticate with an OAuth bearer from `auth` instead of a static key
    /// (#684 edge d) — see the field docs for the pool-identity rule.
    pub fn with_auth(mut self, auth: std::sync::Arc<dyn crate::oauth::AccessTokenSource>) -> Self {
        self.auth = Some(auth);
        self
    }
}

/// Factory for one per-session [`OpenAiLlm`]. Pass the provider's base URL, an
/// optional key, the default model id, the endpoint's rpm budget, the endpoint's
/// concurrency cap (#414), a resolver for the model's own tighter concurrency
/// cap on that endpoint (`|_| None` disables the extra gate, #521; resolved
/// per request rather than once at construction, #550), and the opt-in
/// [`WebSearchConfig`] (`None` disables provider-side web search, #305).
/// `auth = Some(..)` switches the endpoint to an OAuth bearer (#684 edge d),
/// overriding `api_key` on the wire while `api_key` (usually `None` then)
/// keeps its pool-identity role. `thinking_spec` resolves the model's
/// thinking handling per request (ADR-0191) —
/// `Catalog::thinking_spec_resolver(provider)` is the catalog-driven form.
#[allow(clippy::too_many_arguments)]
pub fn openai_factory(
    base_url: impl Into<String>,
    api_key: Option<String>,
    auth: Option<std::sync::Arc<dyn crate::oauth::AccessTokenSource>>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    model_concurrency: ModelConcurrencyResolver,
    web_search: Option<WebSearchConfig>,
    send_prompt_cache_key: bool,
    thinking_spec: crate::ThinkingSpecResolver,
    http: HttpClient,
) -> crate::LlmFactory {
    let mut llm = OpenAiLlm::new(
        base_url,
        api_key,
        default_model,
        rpm,
        concurrency,
        model_concurrency,
        web_search,
        send_prompt_cache_key,
        thinking_spec,
        http,
    );
    if let Some(auth) = auth {
        llm = llm.with_auth(auth);
    }
    std::sync::Arc::new(move || Box::new(llm.clone()) as Box<dyn Llm>)
}

#[async_trait]
impl Llm for OpenAiLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        // Resolved against *this* request's model, not baked in at
        // construction (#550) — a profile's `model:`-only pin can send a
        // request under a different model than `default_model`.
        let model_concurrency = (self.model_concurrency)(&model);
        let thinking = (self.thinking_spec)(&model);
        let think = match thinking.format {
            crate::ThinkingFormat::InlineTags => Some(ThinkSplitter::new()),
            crate::ThinkingFormat::Fields => None,
        };
        let body = request::build_body(
            &model,
            req.system,
            req.messages,
            req.tools,
            req.generation,
            self.web_search.as_ref(),
            req.cache_key.filter(|_| self.send_prompt_cache_key),
            thinking,
            req.trailing_notice.as_deref(),
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

        // An OAuth bearer (#684 edge d) is fetched per request — cached until
        // expiry by the source — and retried exactly once with a forced
        // refresh on a `401` (a revoked-but-unexpired token, mirroring the MCP
        // transport). The pool identity below stays `api_key`, never the
        // bearer (ADR-0156: a rotating credential must not churn buckets).
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
                        anyhow::anyhow!("openai-compat request failed: {e}")
                    }
                    crate::client::RetryError::Exhausted(attempts, e) => anyhow::anyhow!(
                        "openai-compat request failed after {} attempts: {e}",
                        attempts
                    ),
                    crate::client::RetryError::RateLimited => anyhow::anyhow!(
                        "openai-compat rate limited: gave up waiting for the endpoint to clear"
                    ),
                    crate::client::RetryError::HeaderTimeout(timeout, attempts) => anyhow::anyhow!(
                        "openai-compat request failed: no response headers within {timeout:?} \
                         after {attempts} attempt(s)"
                    ),
                })?;
            if response.status().as_u16() == 401 && self.auth.is_some() && !forced_refresh {
                tracing::warn!(
                    base = %self.base_url,
                    "openai-compat 401 with an OAuth bearer; forcing one refresh and retrying"
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
        tracing::debug!(model = %model, base = %self.base_url, "openai-compat stream started");
        Ok(stream::decode(rx, think, model))
    }
}
