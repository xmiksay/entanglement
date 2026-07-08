//! Concrete LLM backends for the entanglement agent engine.
//!
//! This crate is the home for any backend that needs real I/O (HTTP clients,
//! streaming transports) — those are forbidden in `entanglement-core` by ADR-0006's
//! zero-transport-dep rule. Backends implement [`entanglement_core::Llm`] and are
//! handed to [`entanglement_core::EngineConfig`] as a factory.
//!
//! Provider topology (mirrors opencode / the Vercel AI SDK): one generic
//! OpenAI-compatible client for every provider that speaks `/chat/completions`
//! (z.ai GLM — entanglement's primary, OpenAI, Ollama) via [`openai`], and a separate
//! Anthropic client for its distinct `/v1/messages` content-block format via
//! [`anthropic`]. The two formats diverge enough (system as a top-level field;
//! tool results merged into one user turn) to justify separate modules rather
//! than flags on one client.

pub mod anthropic;
pub mod client;
pub mod openai;

pub use anthropic::{anthropic_factory, AnthropicLlm};
pub use client::HttpClient;
pub use openai::{
    openai_factory, OpenAiLlm, OLLAMA_BASE, OPENAI_BASE, ZAI_CODING_PLAN_BASE, ZAI_GENERAL_BASE,
};
