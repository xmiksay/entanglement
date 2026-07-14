//! Headless AI coding agent engine.
//!
//! `entanglement-core` owns the reasoning + tool-execution loop and is strictly
//! decoupled from any UI. The contract is an actor: a [`Holly`] holds an inbox
//! of [`InMsg`] and an outbox of [`OutEvent`]. Every head (in-process ABI,
//! stdio NDJSON, WebSocket, TUI) is a thin adapter over [`Holly::send`] and
//! [`Holly::subscribe`].
//!
//! See `docs/architecture.md` for the design.

pub mod context;
pub mod holly;
pub mod protocol;
pub mod session;

pub use context::Context;
pub use holly::{ConfigError, EngineConfig, Holly, ProfileRegistry};
// The LLM seam (trait + DTOs + wire `Message`) lives in `entanglement-provider`,
// the leaf crate; core depends on it and re-exports for its heads (ADR-0053).
pub use entanglement_provider::{
    content_text, stream_from_response, ContentPart, DummyLlm, EchoLlm, GenerationParams,
    ImageSource, Llm, LlmEvent, LlmFactory, LlmRequest, LlmResponse, LlmStream, Message,
    MessageRole, ModelPricing, ModelResolver, ResolvedModel, StopReason, ToolCall, ToolSpec, Usage,
};
pub use protocol::{
    AgentMode, AgentProfile, AgentState, ApprovalScope, FileChangeKind, InMsg, OutEvent,
    Permission, PermissionProfile, ProfileDetail, QuestionOption, SessionId, SessionInfo,
};
