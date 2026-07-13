//! LLM backend abstraction. The engine talks to an [`Llm`] through a
//! streaming [`LlmRequest`] → [`LlmEvent`] contract: the backend emits
//! incremental text chunks, assembled tool calls, and a terminal `Finish`.
//!
//! Streaming mirrors opencode (which drives the Vercel AI SDK's `doStream`),
//! keeping live token-by-token UI feedback as a first-class concern. Concrete
//! backends live out-of-tree so [`DummyLlm`] is the only in-core implementation;
//! the real Anthropic SSE client lives in the `entanglement-provider` crate (it pulls in
//! `reqwest`, which core must never depend on — see ADR-0006 / ADR-0007).

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

/// One event in a streamed model response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    /// Incremental assistant text (a chunk, not the whole reply).
    Text(String),
    /// Incremental reasoning/thinking text (extended thinking from models).
    Reasoning(String),
    /// A tool the model wants to run, fully assembled (id + name + JSON input).
    ToolCall(ToolCall),
    /// Stream ended cleanly. Carries usage when the provider reports it.
    Finish {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
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

/// Everything the model needs for one completion, drawn from the session's
/// active agent profile + registered tools.
pub struct LlmRequest<'a> {
    pub system: &'a str,
    /// Profile-pinned model id; `None` means "use the backend's default".
    pub model: Option<&'a str>,
    pub messages: &'a [crate::Message],
    pub tools: &'a [ToolSpec],
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
pub type LlmFactory = std::sync::Arc<dyn Fn() -> LlmSession + Send + Sync>;

/// Provider-owned "live session/connection handle" — the object a session holds
/// for its LLM backend, distinct from `Context` (the conversation history), which
/// stays in core. It is a newtype around `Box<dyn Llm>`; the boxed backend
/// carries the provider layer's pool/retry/rate-limit context, which since #217
/// is **keyed per endpoint** (RPM budget + `Retry-After` window) rather than a
/// single global throttle — so the connection state this handle references is
/// isolated per API endpoint.
pub struct LlmSession {
    inner: Box<dyn Llm>,
}

impl LlmSession {
    pub fn new(llm: Box<dyn Llm>) -> Self {
        Self { inner: llm }
    }

    pub fn inner_mut(&mut self) -> &mut dyn Llm {
        &mut *self.inner
    }
}

#[async_trait]
impl Llm for LlmSession {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.inner.stream(req).await
    }
}

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
                input_tokens: None,
                output_tokens: None,
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
                input_tokens: None,
                output_tokens: None,
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
        input_tokens: None,
        output_tokens: None,
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
