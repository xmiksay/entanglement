//! LLM backend abstraction — the seam every consumer talks to. A backend
//! streams a conversation turn through an [`LlmRequest`] → [`LlmEvent`]
//! contract: incremental text chunks, assembled tool calls, and a terminal
//! `Finish`.
//!
//! This trait + its DTOs live in `entanglement-provider` (the leaf crate) so a
//! consumer can issue raw LLM queries against the concrete backends
//! ([`crate::AnthropicLlm`], [`crate::OpenAiLlm`]) without depending on the
//! `entanglement-core` engine. `entanglement-core` depends on this crate and
//! drives `dyn Llm` from its turn loop (see ADR-0053, superseding the original
//! trait-in-core split of ADR-0006 / ADR-0007). Streaming mirrors opencode
//! (which drives the Vercel AI SDK's `doStream`), keeping live token-by-token
//! UI feedback a first-class concern.

mod generation;
mod resolvers;
mod stub;
mod types;

pub use generation::{GenerationParams, ReasoningEffort};
pub use resolvers::{
    fixed_model_concurrency, fixed_thinking_spec, AuxLlmResolver, GenerationResolver, LlmFactory,
    ModelConcurrencyResolver, ModelResolver, ResolvedModel, ThinkingSpecResolver, UserId,
};
pub use stub::{DummyLlm, EchoLlm};
pub use types::{LlmEvent, StopReason, ToolCall, ToolSpec, Usage};

use async_trait::async_trait;
use futures::stream::BoxStream;

/// Everything the model needs for one completion, drawn from the session's
/// active agent profile + registered tools.
pub struct LlmRequest<'a> {
    pub system: &'a str,
    /// Profile-pinned model id; `None` means "use the backend's default".
    pub model: Option<&'a str>,
    pub messages: &'a [crate::Message],
    pub tools: &'a [ToolSpec],
    /// Resolved generation knobs (temperature / max-tokens / thinking budget).
    /// `None` ⇒ the backend's own defaults for every knob (#191).
    pub generation: Option<GenerationParams>,
    /// Stable per-conversation routing hint for provider-side implicit prompt
    /// caching (#673) — the session id, in practice. A wire that has a native
    /// field for it sends it (OpenAI's `prompt_cache_key`, catalog-gated per
    /// provider); every other backend ignores it. `None` on one-shot aux
    /// requests (summarize / session-title), whose prefix shares nothing with
    /// the session's own.
    pub cache_key: Option<&'a str>,
    /// Per-request override of the endpoint's retry/backoff/timeout knobs
    /// (aux fail-fast, #560 follow-up), forwarded verbatim to
    /// [`crate::client::HttpClient::execute_with_retry`]'s own `retry`
    /// parameter. `None` (every primary-turn request) leaves the endpoint's
    /// pooled `RetryConfig` in force; the aux narrate/session-title/summarize
    /// paths set `Some(RetryConfig::minimal())` when they resolved a purpose
    /// pin, so a dead pinned endpoint fails one caller's probe fast instead of
    /// retry-storming through the LLM-tuned ladder on every call.
    pub retry: Option<crate::client::RetryConfig>,
    /// A short, ephemeral line appended after the conversation — e.g. core's
    /// per-round permission-mode notice (ADR-0207 §9) — rendered as the
    /// wire's final user-role turn but **never** part of `messages` and never
    /// cached. Kept out of `messages` deliberately: every provider places its
    /// cache breakpoints by inspecting real history (the last/third-to-last
    /// user turn on Anthropic, ADR-0202/#673), and a value that is last every
    /// round but different every round would anchor there and invalidate the
    /// prefix each request instead of ever landing a hit. Each `build_body`
    /// appends it *after* anchor placement, so it is invisible to that logic.
    /// `None` behaves exactly as if the field did not exist. Owned (not
    /// `&'a str`): every caller builds it fresh from live, non-`'a` state
    /// (e.g. `Session::mode`) each round, so borrowing would just force the
    /// caller to stash an extra local for a one-shot value.
    pub trailing_notice: Option<String>,
}

/// Name of the single MCP envelope tool (ADR-0226): `mcp {server, tool,
/// args}` routes a call to any MCP tool without advertising its schema, so
/// the `tools` array — and the provider's cached prefix — stays stable.
/// Reserved: never a registered tool name. Shared here, in the leaf crate,
/// because core unwraps the envelope and the runtime advertises it.
pub const MCP_TOOL: &str = "mcp";

/// The registered name of MCP tool `tool` on `server`: `mcp__<server>__<tool>`
/// with every character outside `[A-Za-z0-9_-]` mapped to `_`, so it satisfies
/// every wire's tool-name rule whatever a server calls itself. The one rule
/// both core's `mcp` unwrap and the runtime's registration use — two copies
/// could drift and strand a call on a name nothing registered.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A boxed, owned, sendable stream of model events. `'static` so the session
/// loop can hold it across `.await` points without borrowing the backend.
pub type LlmStream = BoxStream<'static, anyhow::Result<LlmEvent>>;

/// Anything that can stream a conversation turn for the engine.
#[async_trait]
pub trait Llm: Send {
    /// Begin a streamed completion. Setup/transport errors (auth, HTTP 4xx,
    /// connection) come back as the `Err`; mid-stream errors arrive as `Err`
    /// items in the returned stream.
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream>;

    /// Non-streaming convenience (#728): run [`stream`](Llm::stream) and drain
    /// it into one [`LlmResponse`](crate::LlmResponse), for callers with no
    /// live display. A stream that errors mid-way fails the whole call — a
    /// partial reply is never returned, since its tool calls may be incomplete.
    /// A stream that ends cleanly without a `Finish` still returns, with
    /// `stop_reason: None`: treat that as an ambiguous stop (ADR-0118), not as
    /// a confident end of turn.
    async fn complete(&mut self, req: LlmRequest<'_>) -> anyhow::Result<crate::LlmResponse> {
        crate::response::collect(self.stream(req).await?).await
    }
}
