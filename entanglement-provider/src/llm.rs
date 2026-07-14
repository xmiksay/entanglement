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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: String,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    /// Incremental assistant text (a chunk, not the whole reply).
    Text(String),
    /// Incremental reasoning/thinking text (extended thinking from models).
    Reasoning(String),
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq)]
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
                stop_reason: None,
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
                stop_reason: None,
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
    let users: Vec<&str> = req
        .messages
        .iter()
        .filter(|m| m.role == crate::MessageRole::User)
        .map(|m| m.text.as_str())
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
    if !resp.text.is_empty() {
        events.push(Ok(LlmEvent::Text(resp.text)));
    }
    for call in resp.tool_calls {
        events.push(Ok(LlmEvent::ToolCall(call)));
    }
    events.push(Ok(LlmEvent::Finish {
        stop_reason: None,
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
}
