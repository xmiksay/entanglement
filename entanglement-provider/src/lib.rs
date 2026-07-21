//! Concrete LLM backends + the provider/model catalog for the entanglement
//! agent engine.

/// The "active model" summary a head carries for display (context bar, sessions
/// list). Built from a [`catalog::ModelEntry`] once a provider + model is
/// resolved; richer metadata (pricing, capability flags) stays in the catalog.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u32>,
}

impl ModelInfo {
    /// Summarize a catalog entry for `model_id` (its context window), falling
    /// back to `model_id` for both id and display name when the id isn't in the
    /// catalog (e.g. an env-override model the user typed).
    pub fn from_catalog(entry: Option<&catalog::ModelEntry>, model_id: &str) -> Self {
        Self {
            id: model_id.to_string(),
            display_name: entry
                .map(|e| e.display_name().to_string())
                .unwrap_or_else(|| model_id.to_string()),
            context_window: entry.and_then(|e| e.context_window),
        }
    }
}

pub mod anthropic;
pub mod catalog;
pub mod client;
pub mod gemini;
pub mod llm;
pub mod message;
pub mod openai;
mod sse_frame;
pub mod web_search;

pub use anthropic::{anthropic_factory, AnthropicLlm};
pub use catalog::{Catalog, ModelEntry, ModelPricing, ProviderEntry, Wire};
pub use client::{HttpClient, RetryConfig, StreamGuard, ThrottleStatus};
pub use gemini::{gemini_factory, GeminiLlm, GEMINI_BASE};
pub use llm::{
    stream_from_response, DummyLlm, EchoLlm, GenerationParams, GenerationResolver, Llm, LlmEvent,
    LlmFactory, LlmRequest, LlmResponse, LlmStream, ModelResolver, ReasoningEffort, ResolvedModel,
    StopReason, ToolCall, ToolSpec, Usage,
};
pub use message::{
    content_has_image, content_text, ContentPart, ImageSource, Message, MessageRole,
};
pub use openai::{
    openai_factory, OpenAiLlm, OLLAMA_BASE, OPENAI_BASE, ZAI_CODING_PLAN_BASE, ZAI_GENERAL_BASE,
};
pub use web_search::WebSearchConfig;
