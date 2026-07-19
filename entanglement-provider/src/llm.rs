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

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;

/// A tool the model asked to run. `input` is the raw JSON argument string.
///
/// `provider_meta` is an opaque, provider-private slot (e.g. Gemini's
/// `thoughtSignature` on a thinking model's function call) that must round-trip
/// **verbatim** through history persistence + replay: the provider stashes it on
/// the way out and restores it when rebuilding the request from history, and core
/// never inspects it. It carries `serde_json::Value` (which is not `Eq`), so
/// `ToolCall` is `PartialEq` but not `Eq`. Persisted with the ADR-0064 back-compat
/// shim (`#[serde(default, skip_serializing_if = …)]`) so pre-#309 logs — which
/// have no `provider_meta` — still deserialize and replay unchanged.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_meta: Option<serde_json::Value>,
}

impl ToolCall {
    /// A tool call with no provider-private metadata — the common case for every
    /// wire that has nothing to round-trip (OpenAI-compat, most Anthropic turns).
    pub fn new(id: impl Into<String>, name: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }
    }
}

/// One tool the engine advertises to the model (name + short description so the
/// model knows when to call it).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object (surfaces as Anthropic's
    /// `input_schema`). Defaults to a permissive empty-object schema.
    pub schema: serde_json::Value,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    pub fn with_schema(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
        }
    }
}

/// Why a model stopped generating, normalized across providers (#192). Both the
/// OpenAI-compat `finish_reason` and Anthropic's `stop_reason` map onto this so a
/// consumer never has to know a provider's wire vocabulary. [`MaxTokens`] is the
/// load-bearing case — a reply truncated by the output cap that would otherwise
/// commit silently as a clean turn.
///
/// [`MaxTokens`]: StopReason::MaxTokens
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Natural completion (OpenAI `stop`, Anthropic `end_turn`).
    EndTurn,
    /// The model wants to run tools (OpenAI `tool_calls`, Anthropic `tool_use`).
    ToolUse,
    /// Output was cut off at the max-token limit (OpenAI `length`, Anthropic
    /// `max_tokens`) — the reply is truncated.
    MaxTokens,
    /// A configured stop sequence matched (Anthropic `stop_sequence`).
    StopSequence,
    /// Anything else the provider reports (e.g. `content_filter`) or an
    /// unrecognized token.
    Other,
}

impl StopReason {
    /// Map an OpenAI-compat `finish_reason` string onto the normalized reason.
    pub fn from_openai(reason: &str) -> Self {
        match reason {
            "stop" => StopReason::EndTurn,
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::Other,
        }
    }

    /// Map an Anthropic `stop_reason` string onto the normalized reason.
    pub fn from_anthropic(reason: &str) -> Self {
        match reason {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::Other,
        }
    }

    /// Map a Gemini `finishReason` string onto the normalized reason. Gemini has
    /// no distinct tool-use reason — it reports `STOP` even when the turn is
    /// function calls — so the caller upgrades `EndTurn` to [`ToolUse`] when it
    /// actually emitted a tool call this turn.
    ///
    /// [`ToolUse`]: StopReason::ToolUse
    pub fn from_gemini(reason: &str) -> Self {
        match reason {
            "STOP" => StopReason::EndTurn,
            "MAX_TOKENS" => StopReason::MaxTokens,
            _ => StopReason::Other,
        }
    }

    /// A confident, deliberate stop — the only kind that ends a turn with no
    /// tool calls (ADR-0118). `session::turn::run_round` treats every other
    /// reason (`ToolUse` with zero actual tool calls, or `Other`) as ambiguous
    /// and retries instead of ending the turn; a caller with no `StopReason` at
    /// all (a stream that closed without reporting one) treats that as
    /// ambiguous too. Written as an exhaustive `match` (no `_` wildcard) so
    /// adding a variant here is a compile error until it is explicitly
    /// classified, instead of silently defaulting to ambiguous.
    pub fn is_confident_stop(self) -> bool {
        match self {
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => true,
            StopReason::ToolUse | StopReason::Other => false,
        }
    }
}

/// Normalized token usage for one model round-trip (#192). Every field is
/// optional — a provider may not report a given dimension. Counts are normalized
/// so each maps to exactly one catalog pricing dimension without double-counting:
/// `input_tokens` is the *uncached* input (OpenAI's `prompt_tokens` minus its
/// cached reads; Anthropic already reports these separately), `cached_input_tokens`
/// is the cache-read portion, and `cache_write_tokens` is the cache-creation
/// portion (Anthropic only; OpenAI does not bill cache writes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

/// One event in a streamed model response.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    /// Incremental assistant text (a chunk, not the whole reply).
    Text(String),
    /// Incremental reasoning/thinking text (extended thinking from models).
    Reasoning(String),
    /// Incremental tool-call argument fragment (#194), streamed as the model
    /// emits a tool call's JSON input *before* the assembled [`ToolCall`].
    /// Correlated to that final call by `id`; `name` rides every fragment so a
    /// head can label the stream before the args finish. `delta` is a raw
    /// substring of the JSON argument text — the fragments concatenated in
    /// arrival order rebuild [`ToolCall::input`]. Additive: a consumer that only
    /// needs the assembled call can ignore it and still get the terminal
    /// [`ToolCall`].
    ToolCallDelta {
        id: String,
        name: String,
        delta: String,
    },
    /// A tool the model wants to run, fully assembled (id + name + JSON input).
    ToolCall(ToolCall),
    /// Stream ended cleanly. Carries the normalized [`StopReason`] and [`Usage`]
    /// when the provider reports them (#192).
    Finish {
        stop_reason: Option<StopReason>,
        usage: Usage,
    },
}

/// A fully-formed model reply — text plus any tool calls. Used by scripted test
/// backends ([`DummyLlm`] ignores it); NOT part of the [`Llm`] trait, which is
/// streaming-only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlmResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Per-request generation knobs, resolved by the head from the effective model's
/// catalog capabilities + agent profile (#191). This is the channel that makes
/// the catalog's capability metadata (`supports_temperature`/`default_temperature`/
/// `max_output_tokens`/`supports_thinking`) load-bearing instead of write-only:
/// the head only populates a field when the model actually supports it, and each
/// client maps the present fields to its wire format, omitting the rest.
///
/// Every field is optional — a `None` (or a `None` [`LlmRequest::generation`])
/// leaves that knob at the backend's own default.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GenerationParams {
    /// Sampling temperature. `None` ⇒ omit (the model's default), which the head
    /// also does for a model with `supports_temperature: false`.
    pub temperature: Option<f32>,
    /// Hard cap on tokens generated this turn. OpenAI-compat sends it as
    /// `max_tokens`; Anthropic (which *requires* a cap) uses it in place of its
    /// built-in fallback. `None` ⇒ the client's own default.
    pub max_output_tokens: Option<u32>,
    /// Extended-thinking budget in tokens (Anthropic `thinking.budget_tokens`).
    /// `None` — or a model with `supports_thinking: false` — leaves thinking off.
    /// Wires without a thinking channel (OpenAI-compat) omit it.
    pub thinking_budget_tokens: Option<u32>,
    /// Coarse reasoning-effort knob (#374) — OpenAI's native `reasoning_effort`
    /// wire field. Anthropic has no effort concept of its own; its client maps
    /// this onto a thinking budget (see `anthropic.rs`'s `build_body`). Gemini
    /// maps it onto `thinkingConfig.thinkingBudget` the same way. `None` leaves
    /// the knob unset.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl GenerationParams {
    /// Overlay `overrides` onto `self`, field by field — each `Some` in
    /// `overrides` replaces the corresponding field, a `None` leaves `self`'s
    /// value untouched. The merge primitive behind a partial
    /// `InMsg::SetGeneration { overrides, .. }` (#374): `/set temperature 0.7`
    /// only touches `temperature`, leaving `max_output_tokens`/
    /// `thinking_budget_tokens`/`reasoning_effort` exactly as they were.
    pub fn apply_overrides(&mut self, overrides: GenerationParams) {
        if overrides.temperature.is_some() {
            self.temperature = overrides.temperature;
        }
        if overrides.max_output_tokens.is_some() {
            self.max_output_tokens = overrides.max_output_tokens;
        }
        if overrides.thinking_budget_tokens.is_some() {
            self.thinking_budget_tokens = overrides.thinking_budget_tokens;
        }
        if overrides.reasoning_effort.is_some() {
            self.reasoning_effort = overrides.reasoning_effort;
        }
    }
}

/// Coarse reasoning-effort knob (#374): OpenAI's native `reasoning_effort` wire
/// value (`low|medium|high`, hence `rename_all = "lowercase"` rather than
/// Rust's usual `PascalCase`). Anthropic and Gemini have no such field — each
/// client maps it onto a thinking-budget tier instead (documented at their
/// `build_body`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[cfg(test)]
mod generation_params_tests {
    use super::*;

    #[test]
    fn apply_overrides_touches_only_present_fields() {
        let mut base = GenerationParams {
            temperature: Some(0.2),
            max_output_tokens: Some(1024),
            thinking_budget_tokens: None,
            reasoning_effort: None,
        };
        base.apply_overrides(GenerationParams {
            temperature: Some(0.7),
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: Some(ReasoningEffort::High),
        });
        assert_eq!(base.temperature, Some(0.7));
        assert_eq!(base.max_output_tokens, Some(1024)); // untouched
        assert_eq!(base.thinking_budget_tokens, None);
        assert_eq!(base.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn apply_overrides_of_default_is_a_no_op() {
        let mut base = GenerationParams {
            temperature: Some(0.2),
            max_output_tokens: Some(1024),
            thinking_budget_tokens: Some(4096),
            reasoning_effort: Some(ReasoningEffort::Low),
        };
        let before = base;
        base.apply_overrides(GenerationParams::default());
        assert_eq!(base, before);
    }

    #[test]
    fn reasoning_effort_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ReasoningEffort::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::from_str::<ReasoningEffort>("\"medium\"").unwrap(),
            ReasoningEffort::Medium
        );
    }
}

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
}

/// Factory that produces a fresh per-session LLM instance. Sessions run
/// concurrently, so each gets its own (cheaply-clonable) backend.
///
/// A session simply owns its `Box<dyn Llm>`; there is deliberately **no
/// per-session wrapper**. The provider layer's resilience state (connection pool,
/// retry/backoff, RPM budget + `Retry-After` window) is **keyed per endpoint**,
/// not per session, since #217 / [ADR-0050]: sessions talking to the same endpoint
/// share one budget so a throttled endpoint doesn't starve siblings. There is thus
/// no honest session-scoped state for a handle to hold — the earlier `LlmSession`
/// newtype was an empty placeholder and was collapsed away (#195 /
/// [ADR-0062]). Re-introduce the newtype only when genuinely per-session state
/// (e.g. a session-pinned model override or conversation-scoped budget) arrives.
///
/// [ADR-0050]: ../../docs/adr/0050-per-endpoint-connection-pool-retry-rate-limit.md
/// [ADR-0062]: ../../docs/adr/0062-collapse-llmsession-placeholder-newtype.md
pub type LlmFactory = std::sync::Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>;

/// Everything a live session needs to re-bind itself to a different
/// model/provider without an engine restart (#218): the factory that builds the
/// new `Box<dyn Llm>`, the effective model id + provider it resolved to, and the
/// per-model generation knobs + context window that must follow the switch. The
/// runtime resolves it from the catalog + user config (reusing the same wire /
/// base / key resolution as startup); core applies it to a running session on
/// a `SetModel` message — rebuilding `Session::llm`, retargeting the request
/// model, re-budgeting the context window.
#[derive(Clone)]
pub struct ResolvedModel {
    /// Catalog provider name the switch landed on (canonical, from the entry).
    pub provider: String,
    /// The effective model id — sent on every subsequent request and used to
    /// price the turn.
    pub model: String,
    /// Builds the new per-session backend bound to `provider`/`model`.
    pub llm_factory: LlmFactory,
    /// Generation knobs for `model` (temperature / max-output / thinking), gated
    /// by its catalog capabilities. `None` for a model absent from the catalog.
    pub generation: Option<GenerationParams>,
    /// `model`'s context window in tokens, so the session re-budgets its history
    /// against the real window. `None` (unknown model) falls back to the flat cap.
    pub context_window: Option<usize>,
}

/// Re-resolves a `(provider, model)` pair to a [`ResolvedModel`] for a
/// mid-session switch (#218), or `Err(message)` when the provider is unknown or
/// its API key is unset. Held by the engine config so a session can swap its LLM
/// with no restart; the runtime builds it capturing the provider catalog + the
/// per-endpoint HTTP client (already warm, #217).
pub type ModelResolver =
    std::sync::Arc<dyn Fn(&str, &str) -> Result<ResolvedModel, String> + Send + Sync>;

/// Resolves a named agent profile's **persisted** generation override (#374,
/// the generation-parameter analogue of the model pin ADR-0081 bakes directly
/// into `AgentProfile.provider`/`model`). [`GenerationParams`] carries a
/// non-`Eq` `f32` (`temperature`), so it can't join `AgentProfile`'s
/// `PartialEq + Eq` derive the way the pin fields do — this resolver is a
/// separate seam instead, mirroring [`ModelResolver`]'s shape but purely local
/// (a managed-file lookup, no network/key validation), hence `Option` rather
/// than `Result`. Held by the engine config; the runtime supplies it wrapping
/// its `AgentGenerationStore`. `None` (the default, or a lookup miss) means the
/// profile carries no persisted override — the session keeps its current
/// generation binding.
pub type GenerationResolver =
    std::sync::Arc<dyn Fn(&str) -> Option<GenerationParams> + Send + Sync>;

/// Deterministic stub backend. Emits a configured reply as a single text chunk
/// then `Finish` — ideal for bootstrap wiring and unit tests.
pub struct DummyLlm {
    reply: String,
}

impl DummyLlm {
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
        }
    }
}

impl Default for DummyLlm {
    fn default() -> Self {
        Self::new("(dummy) thinking...")
    }
}

#[async_trait]
impl Llm for DummyLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let events = vec![
            Ok(LlmEvent::Text(self.reply.clone())),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

/// Echo-mode stub backend. Returns a text summary of the request it received,
/// making prompt assembly observable without a real provider. The reply reports
/// the total message count, every user-text snippet, the assembled system
/// prompt (its byte length + an 8-hex SHA-256 fingerprint) and the advertised
/// tool names — so a test (or a human in the TUI) can verify at a glance which
/// prompt/tool set actually reached the backend, not just whether prior turns
/// survived. Set `ENTANGLEMENT_ECHO_FULL=1` to append the full system text.
pub struct EchoLlm;

impl EchoLlm {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EchoLlm {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Llm for EchoLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let reply = echo_reply(
            &req,
            std::env::var("ENTANGLEMENT_ECHO_FULL").as_deref() == Ok("1"),
        );
        let events = vec![
            Ok(LlmEvent::Text(reply)),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

/// Render the [`EchoLlm`] summary line for a request: message count, user-text
/// snippets, the system-prompt length + fingerprint, and the advertised tool
/// names. With `full`, the full system text is appended on its own line.
fn echo_reply(req: &LlmRequest<'_>, full: bool) -> String {
    let total = req.messages.len();
    let users: Vec<String> = req
        .messages
        .iter()
        .filter(|m| m.role == crate::MessageRole::User)
        .map(|m| m.text())
        .collect();
    let tools: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
    let mut reply = format!(
        "echo: messages={total}, users={users:?}, system_len={}, system_sha={}, tools={tools:?}",
        req.system.len(),
        sha8(req.system),
    );
    if full {
        reply.push_str("\nsystem:\n");
        reply.push_str(req.system);
    }
    reply
}

/// First 8 hex chars of the SHA-256 of `s` — a short, stable fingerprint of the
/// system prompt so callers can diff assemblies without echoing the full text.
fn sha8(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(8);
    for byte in &digest[..4] {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Build a one-shot stream from a full [`LlmResponse`] (text + tool calls then
/// `Finish`). Convenience for scripted/test backends.
pub fn stream_from_response(resp: LlmResponse) -> LlmStream {
    let mut events: Vec<anyhow::Result<LlmEvent>> = Vec::with_capacity(resp.tool_calls.len() + 2);
    let stop_reason = if resp.tool_calls.is_empty() {
        StopReason::EndTurn
    } else {
        StopReason::ToolUse
    };
    if !resp.text.is_empty() {
        events.push(Ok(LlmEvent::Text(resp.text)));
    }
    for call in resp.tool_calls {
        events.push(Ok(LlmEvent::ToolCall(call)));
    }
    events.push(Ok(LlmEvent::Finish {
        stop_reason: Some(stop_reason),
        usage: Usage::default(),
    }));
    stream::iter(events).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    fn req<'a>(system: &'a str, messages: &'a [Message], tools: &'a [ToolSpec]) -> LlmRequest<'a> {
        LlmRequest {
            system,
            model: None,
            messages,
            tools,
            generation: None,
        }
    }

    #[test]
    fn legacy_tool_call_without_provider_meta_deserializes() {
        // A tool call persisted before #309 has no `provider_meta` field; the
        // `#[serde(default)]` shim must still deserialize it (→ None) so old logs
        // replay unchanged.
        let legacy = r#"{"id":"c1","name":"read","input":"{}"}"#;
        let tc: ToolCall = serde_json::from_str(legacy).unwrap();
        assert_eq!(tc.id, "c1");
        assert_eq!(tc.provider_meta, None);
    }

    #[test]
    fn provider_meta_roundtrips_and_is_omitted_when_none() {
        // None → the field is skipped on the wire (byte-identical to a pre-#309 log).
        let plain = ToolCall::new("c1", "read", "{}");
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"id":"c1","name":"read","input":"{}"}"#
        );
        // Some(..) → round-trips verbatim.
        let with_meta = ToolCall {
            provider_meta: Some(serde_json::json!({ "sig": "abc" })),
            ..ToolCall::new("c2", "search", "{}")
        };
        let json = serde_json::to_string(&with_meta).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_meta);
    }

    #[test]
    fn sha8_is_eight_lowercase_hex() {
        let h = sha8("hello");
        assert_eq!(h.len(), 8);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // SHA-256("hello") starts with 2cf24dba…
        assert_eq!(h, "2cf24dba");
    }

    #[test]
    fn echo_reply_reports_system_and_tools() {
        let messages = [Message::user("hi"), Message::assistant("prev", vec![])];
        let tools = [
            ToolSpec::new("read", "read a file"),
            ToolSpec::new("bash", "run"),
        ];
        let out = echo_reply(&req("you are a bot", &messages, &tools), false);

        assert!(out.contains("messages=2"), "{out}");
        assert!(out.contains("users=[\"hi\"]"), "{out}");
        assert!(out.contains("system_len=13"), "{out}");
        assert!(out.contains("system_sha="), "{out}");
        assert!(out.contains("tools=[\"read\", \"bash\"]"), "{out}");
        // The full system text stays hidden unless explicitly requested.
        assert!(!out.contains("\nsystem:\n"), "{out}");
    }

    #[test]
    fn echo_reply_full_appends_system_text() {
        let out = echo_reply(&req("SECRET-PROMPT", &[], &[]), true);
        assert!(out.contains("system_len=13"), "{out}");
        assert!(out.ends_with("\nsystem:\nSECRET-PROMPT"), "{out}");
    }

    #[test]
    fn is_confident_stop_matches_adr_0118_classification() {
        // EndTurn/MaxTokens/StopSequence are deliberate, confident stops.
        assert!(StopReason::EndTurn.is_confident_stop());
        assert!(StopReason::MaxTokens.is_confident_stop());
        assert!(StopReason::StopSequence.is_confident_stop());
        // ToolUse (a contradictory reason when no tool calls actually landed)
        // and Other are ambiguous.
        assert!(!StopReason::ToolUse.is_confident_stop());
        assert!(!StopReason::Other.is_confident_stop());
    }
}
