//! Concrete LLM backends for the brain agent engine.
//!
//! This crate is the home for any backend that needs real I/O (HTTP clients,
//! streaming transports) — those are forbidden in `brain-core` by ADR-0006's
//! zero-transport-dep rule. Backends implement [`brain_core::Llm`] and are
//! handed to [`brain_core::EngineConfig`] as a factory.
//!
//! Provider topology (mirrors opencode / the Vercel AI SDK): one generic
//! OpenAI-compatible client for every provider that speaks `/chat/completions`
//! (z.ai GLM — brain's primary, OpenAI, Ollama) via [`openai`], and a separate
//! Anthropic client for its distinct `/v1/messages` content-block format via
//! [`anthropic`]. The two formats diverge enough (system as a top-level field;
//! tool results merged into one user turn) to justify separate modules rather
//! than flags on one client.

pub mod anthropic;
pub mod openai;

pub use anthropic::{anthropic_factory, AnthropicLlm};
pub use openai::{
    openai_factory, OpenAiLlm, OLLAMA_BASE, OPENAI_BASE, ZAI_CODING_PLAN_BASE, ZAI_GENERAL_BASE,
};
