//! Headless AI coding agent engine.
//!
//! `brain-core` owns the reasoning + tool-execution loop and is strictly
//! decoupled from any UI. The contract is an actor: a [`Brain`] holds an inbox
//! of [`InMsg`] and an outbox of [`OutEvent`]. Every head (in-process ABI,
//! stdio NDJSON, WebSocket, TUI) is a thin adapter over [`Brain::send`] and
//! [`Brain::subscribe`].
//!
//! See `PLAN.md` and `docs/architecture.md` for the design.

pub mod brain;
pub mod context;
pub mod llm;
pub mod protocol;
pub mod session;
pub mod tools;

pub use brain::{Brain, EngineConfig, ProfileRegistry};
pub use context::{Message, MessageRole};
pub use llm::{DummyLlm, Llm, LlmFactory, LlmRequest, LlmResponse, ToolCall, ToolSpec};
pub use protocol::{
    AgentMode, AgentProfile, AgentState, InMsg, OutEvent, Permission, PermissionProfile, SessionId,
    TaskItem, TaskStatus,
};
pub use tools::{Tool, ToolRegistry};
