//! LLM backend abstraction. The engine talks to an [`Llm`] through an
//! [`LlmRequest`] that carries the active agent profile's system prompt and the
//! available tool list. The real Anthropic streaming client lands in a later
//! phase; [`DummyLlm`] lets the loop run end-to-end with zero networking.

use async_trait::async_trait;

/// A tool the model asked to run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: String,
}

/// A non-streaming model response. Streaming refinement (emit `TextDelta` per
/// chunk) replaces this later without changing the engine's shape much.
#[derive(Debug, Clone, Default)]
pub struct LlmResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// One tool the engine advertises to the model (name + short description so the
/// model knows when to call it).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// Everything the model needs for one completion, drawn from the session's
/// active agent profile + registered tools.
pub struct LlmRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [crate::Message],
    pub tools: &'a [ToolSpec],
}

/// Anything that can complete a conversation turn for the engine.
#[async_trait]
pub trait Llm: Send {
    async fn complete(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmResponse>;
}

/// Factory that produces a fresh per-session LLM instance. Sessions run
/// concurrently, so each gets its own (cheaply-clonable) backend.
pub type LlmFactory = std::sync::Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>;

/// Deterministic stub backend. Echoes a configured reply and never calls tools
/// — ideal for bootstrap wiring and unit tests.
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
    async fn complete(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmResponse> {
        Ok(LlmResponse {
            text: self.reply.clone(),
            tool_calls: Vec::new(),
        })
    }
}
