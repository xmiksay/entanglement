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
use crate::{Llm, LlmEvent, LlmRequest, LlmStream, ModelConcurrencyResolver, StopReason, Usage};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;

/// Default Gemini generative-language base (the `models` collection root).
pub const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Key under which the opaque Gemini `thoughtSignature` is stashed in
/// [`ToolCall::provider_meta`], so restore reads back exactly what stream wrote.
pub(crate) const THOUGHT_SIGNATURE_KEY: &str = "gemini_thought_signature";

/// Build a per-stream-unique [`ToolCall::id`] for a Gemini `functionCall`
/// (#444). Gemini itself has no call id — matching a `functionResponse` back
/// to its call is done by **name** — so two parallel calls to the same tool
/// would otherwise share one id, and the runtime's `request_id` dedupe (the
/// ADR-0071 re-offer soundness mechanism) collapses them into a single
/// `ToolExec`, wedging the turn. `#` can't appear in a Gemini function name
/// (`^[a-zA-Z0-9_.-]+$`), so it's a safe, unambiguous separator to split back
/// off in [`tool_name_from_id`].
fn synthesize_tool_call_id(name: &str, ordinal: usize) -> String {
    format!("{name}#{ordinal}")
}

/// Recover the bare tool name from an id built by [`synthesize_tool_call_id`],
/// for the `functionResponse.name` Gemini matches results by (#444).
pub(crate) fn tool_name_from_id(id: &str) -> &str {
    id.rsplit_once('#').map_or(id, |(name, _)| name)
}

/// Streaming Gemini client. Cheap to clone (the HTTP client is `Arc`-shared
/// internally); build one per session via [`gemini_factory`].
#[derive(Clone)]
pub struct GeminiLlm {
    base_url: String,
    api_key: String,
    /// OAuth bearer source (#684 edge d): `Some` replaces `x-goog-api-key`
    /// with `Authorization: Bearer <token>` fetched per request (cached until
    /// expiry by the source; the context-cache call reuses the same token),
    /// with one forced-refresh retry on a `401`. The endpoint-pool identity
    /// becomes `None` then — a rotating bearer must never key the pool
    /// (ADR-0156).
    auth: Option<std::sync::Arc<dyn crate::mcp::auth::AccessTokenSource>>,
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
    http: HttpClient,
    /// Resolved `cachedContents` resource for the current system+tools
    /// prefix (#587) — shared across every turn this session's clone makes.
    cache: cache::CacheHandle,
}

impl GeminiLlm {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        model_concurrency: ModelConcurrencyResolver,
        http: HttpClient,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            auth: None,
            default_model: default_model.into(),
            rpm,
            concurrency,
            model_concurrency,
            http,
            cache: cache::CacheHandle::new(),
        }
    }

    /// Authenticate with an OAuth bearer from `auth` instead of the static
    /// `x-goog-api-key` (#684 edge d) — see the field docs for the
    /// pool-identity rule. The `api_key` passed at construction is ignored on
    /// the wire then (pass an empty string for a purely OAuth endpoint).
    pub fn with_auth(
        mut self,
        auth: std::sync::Arc<dyn crate::mcp::auth::AccessTokenSource>,
    ) -> Self {
        self.auth = Some(auth);
        self
    }

    /// The per-request bearer, when this client authenticates via OAuth.
    async fn bearer(&self, force_refresh: bool) -> anyhow::Result<Option<String>> {
        match &self.auth {
            Some(source) => Ok(Some(source.access_token(force_refresh).await.map_err(
                |e| anyhow::anyhow!("fetching the OAuth token for `{}`: {e:#}", self.base_url),
            )?)),
            None => Ok(None),
        }
    }
}

/// Build an [`LlmFactory`] wired to Gemini. Each session gets its own cloned
/// [`GeminiLlm`]. `rpm`/`concurrency = None` use the client's (or endpoint's)
/// defaults; `model_concurrency` resolves a tighter per-model cap per request
/// (`|_| None` disables it, #521, resolved per request rather than once at
/// construction, #550).
/// `auth = Some(..)` switches the endpoint to an OAuth bearer (#684 edge d),
/// replacing `x-goog-api-key` on the wire (pass an empty `api_key` then).
#[allow(clippy::too_many_arguments)]
pub fn gemini_factory(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    auth: Option<std::sync::Arc<dyn crate::mcp::auth::AccessTokenSource>>,
    default_model: impl Into<String>,
    rpm: Option<u32>,
    concurrency: Option<usize>,
    model_concurrency: ModelConcurrencyResolver,
    http: HttpClient,
) -> crate::LlmFactory {
    let mut llm = GeminiLlm::new(
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

/// The request auth header: the OAuth bearer when one is in play (#684),
/// else the static Gemini API key.
fn auth_header(bearer: &Option<String>, api_key: &str) -> (&'static str, String) {
    match bearer {
        Some(token) => ("authorization", format!("Bearer {token}")),
        None => ("x-goog-api-key", api_key.to_string()),
    }
}

#[async_trait]
impl Llm for GeminiLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let model = req.model.unwrap_or(&self.default_model).to_string();
        // Resolved against *this* request's model, not baked in at
        // construction (#550) — a profile's `model:`-only pin can send a
        // request under a different model than `default_model`.
        let model_concurrency = (self.model_concurrency)(&model);
        // An OAuth bearer (#684 edge d) is fetched per request — cached until
        // expiry by the source — replacing `x-goog-api-key` on both the
        // generate call and the cache call below.
        let mut bearer = self.bearer(false).await?;
        // Best-effort context caching (#587): reuse or create a
        // `cachedContents` resource for the stable system+tools prefix so it
        // isn't re-billed at the full input rate on every turn, mirroring
        // Anthropic's `cache_control` breakpoints (#566). Never blocks the
        // turn on failure — `resolve` falls back to `None`.
        let cached_content = self
            .cache
            .resolve(
                &self.http,
                &self.base_url,
                &auth_header(&bearer, &self.api_key),
                &model,
                req.system,
                req.tools,
            )
            .await;
        let body = build_body(
            req.system,
            req.messages,
            req.tools,
            req.generation,
            cached_content.as_deref(),
        );
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
        // With OAuth the identity is `None` — a rotating bearer must never key
        // the pool (ADR-0156). One forced-refresh retry on a `401`.
        let mut forced_refresh = false;
        let (response, guard) = loop {
            let (name, value) = auth_header(&bearer, &self.api_key);
            let pool_identity = if self.auth.is_some() {
                None
            } else {
                Some(self.api_key.as_str())
            };
            let (response, guard) = self
                .http
                .execute_with_retry(
                    base,
                    pool_identity,
                    self.rpm,
                    self.concurrency,
                    &model,
                    model_concurrency,
                    None,
                    || {
                        self.http
                            .client()
                            .post(&url)
                            .header(name, &value)
                            .header("content-type", "application/json")
                            .json(&body)
                            .send()
                    },
                )
                .await
                // `RetryError`'s own `Display` (thiserror) already carries the
                // per-variant detail (attempts, elapsed timeout, ...); just
                // prefix it with the provider so mixed-provider logs stay
                // attributable.
                .map_err(|e| anyhow::anyhow!("gemini request failed: {e}"))?;
            if response.status().as_u16() == 401 && self.auth.is_some() && !forced_refresh {
                tracing::warn!("gemini 401 with an OAuth bearer; forcing one refresh and retrying");
                forced_refresh = true;
                bearer = self.bearer(true).await?;
                continue;
            }
            break (response, guard);
        };

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
            // Byte-buffered framing (#443): a multi-byte UTF-8 character can
            // straddle two network chunks, so decoding must wait for a complete
            // `\n\n`-terminated frame — see `sse_frame::SseFrameBuffer`.
            let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n\n");
            let mut usage = Usage::default();
            let mut finish_reason: Option<String> = None;
            let mut saw_tool_call = false;
            let mut tool_call_ordinal: usize = 0;
            let mut rx = rx;

            while let Some(item) = rx.recv().await {
                let chunk = item?;
                frames.push(&chunk);
                while let Some(frame_owned) = frames.next_frame() {
                    let Some(data) = parse_frame(&frame_owned) else { continue };
                    for ev in handle_chunk(&data, &mut usage, &mut finish_reason, &mut tool_call_ordinal)? {
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

mod cache;
mod request;
mod sse;
use request::build_body;
// Private re-imports keep `super::parse_frame`/`super::handle_chunk` valid for
// `gemini/tests.rs` across the split.
use sse::{handle_chunk, parse_frame};

#[cfg(test)]
mod tests;
