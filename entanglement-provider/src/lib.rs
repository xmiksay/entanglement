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
pub mod debug;
pub mod endpoint;
pub mod gemini;
pub mod llm;
pub mod mcp;
pub mod message;
// Per-user provider context + model resolver for multi-user embedders
// (ADR-0181/ADR-0184, #687): `UserId` and everything keyed by it lives in
// this crate, never in the runtime.
pub mod multi_user;
pub mod oauth;
pub mod openai;
pub mod openai_responses;
pub mod provider_mcp;
pub mod response;
mod sse_frame;
pub mod web_fetch;
pub mod web_search;

pub use anthropic::{anthropic_factory, AnthropicLlm, ANTHROPIC_BASE};
pub use catalog::{
    AnthropicModelSpec, Catalog, EffortTiers, ModelEntry, ModelPricing, ProviderEntry,
    ProviderMcpServer, ThinkingControl, ThinkingFormat, ThinkingSpec, ThinkingStyle, Wire,
};
pub use client::{
    prune_stale, HttpClient, RetryConfig, SharedStateLocation, StreamGuard, ThrottleStatus,
    UserBudget,
};
pub use debug::{DebugFlags, DEBUG_ENV};
pub use endpoint::{
    call as call_endpoint, EndpointMethod, EndpointResponse, ENDPOINT_RESPONSE_CAP,
};
pub use gemini::{gemini_factory, GeminiLlm, GEMINI_BASE};
pub use llm::{
    fixed_model_concurrency, fixed_thinking_spec, mcp_tool_name, AuxLlmResolver, DummyLlm, EchoLlm,
    GenerationParams, GenerationResolver, Llm, LlmEvent, LlmFactory, LlmRequest, LlmStream,
    ModelConcurrencyResolver, ModelResolver, ReasoningEffort, ResolvedModel, StopReason,
    ThinkingSpecResolver, ToolCall, ToolSpec, Usage, UserId, MCP_TOOL,
};
// The MCP client mechanism (ADR-0153). `HttpClient` above is the *LLM* endpoint
// client, so the MCP transport keeps its qualified `McpHttpClient` name rather
// than colliding with it at the crate root.
pub use mcp::{
    is_response, jsonrpc_payload, list_tools_paginated, parse_tool_def, server_request_reply,
    McpHttpClient, McpToolDef,
};
pub use message::{
    content_has_image, content_text, tool_reference_fallback_text, ContentPart, ImageSource,
    Message, MessageRole,
};
pub use multi_user::{
    build_user_model_resolver, InMemoryUserProviderStore, UserProviderContext, UserProviderStore,
};
pub use oauth::{
    auth_required_of, is_auth_required, user_scoped, AccessTokenSource, AuthFlow, AuthOutcome,
    AuthRequired, InMemoryUserTokenStore, OauthConfig, PendingWebAuthorization, StoredAuth,
    StoredTokenSource, TokenSet, TokenStore, UserTokenStore, WebFlow,
};
pub use openai::{
    openai_factory, OpenAiLlm, OLLAMA_BASE, OPENAI_BASE, ZAI_CODING_PLAN_BASE, ZAI_GENERAL_BASE,
};
pub use openai_responses::{openai_responses_factory, OpenAiResponsesLlm, OPENAI_RESPONSES_BASE};
pub use response::{stream_from_response, LlmResponse};
pub use web_search::WebSearchConfig;
