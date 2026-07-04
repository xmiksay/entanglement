//! Concrete LLM backends for the brain agent engine.
//!
//! This crate is the home for any backend that needs real I/O (HTTP clients,
//! streaming transports) — those are forbidden in `brain-core` by ADR-0006's
//! zero-transport-dep rule. Backends implement [`brain_core::Llm`] and are
//! handed to [`brain_core::EngineConfig`] as a factory.
//!
//! Today: Anthropic (`/v1/messages`, `stream: true`) via [`anthropic`].

pub mod anthropic;

pub use anthropic::{anthropic_factory, AnthropicLlm};
