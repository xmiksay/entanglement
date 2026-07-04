//! Headless AI coding agent engine.
//!
//! `entanglement-core` owns the reasoning + tool-execution loop and is strictly
//! decoupled from any UI. The contract is an actor: a [`Holly`] holds an inbox
//! of [`InMsg`] and an outbox of [`OutEvent`]. Every head (in-process ABI,
//! stdio NDJSON, WebSocket, TUI) is a thin adapter over [`Holly::send`] and
//! [`Holly::subscribe`].
//!
//! See `PLAN.md` and `docs/architecture.md` for the design.

pub mod context;
pub mod holly;
pub mod host;
pub mod llm;
pub mod protocol;
pub mod session;
pub mod tools;

pub use context::{Message, MessageRole};
pub use holly::{EngineConfig, Holly, ProfileRegistry};
pub use host::host_tools;
pub use llm::{
    stream_from_response, DummyLlm, Llm, LlmEvent, LlmFactory, LlmRequest, LlmResponse, LlmStream,
    ToolCall, ToolSpec,
};
pub use protocol::{
    AgentMode, AgentProfile, AgentState, InMsg, OutEvent, Permission, PermissionProfile, SessionId,
    TaskItem, TaskStatus,
};
pub use tools::{Tool, ToolRegistry};
