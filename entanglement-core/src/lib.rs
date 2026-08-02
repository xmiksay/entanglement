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
pub use holly::{
    ConfigError, EngineConfig, Holly, ProfileRegistry, SystemPromptResolver, ToolSpecResolver,
    WireError,
};
// The LLM seam (trait + DTOs + wire `Message`) lives in `entanglement-provider`,
// the leaf crate; core depends on it and re-exports for its heads (ADR-0053).
pub use entanglement_provider::{
    content_text, stream_from_response, AuxLlmResolver, Catalog, ContentPart, DummyLlm, EchoLlm,
    GenerationParams, GenerationResolver, HttpClient, ImageSource, Llm, LlmEvent, LlmFactory,
    LlmRequest, LlmResponse, LlmStream, McpServerState, Message, MessageRole, ModelPricing,
    ModelResolver, ProviderMcpServer, ReasoningEffort, ResolvedModel, StopReason, ToolCall,
    ToolSpec, Usage, UserId, WebSearchConfig,
};
// The MCP client mechanism — transport + OAuth (ADR-0153) — also lives in the
// leaf crate. Core carries no MCP *logic* (ADR-0067): this is a pass-through so
// the runtime reaches these types on the same path `McpServerState` already
// takes, including in the lean build that names no provider dependency of its
// own. Nothing in core calls them. `HttpClient` above (#559) is what the MCP
// HTTP transport is handed so its traffic shares the same per-endpoint
// pool/RPM/concurrency/retry as LLM traffic — not `McpHttpClient`'s own name.
pub use entanglement_provider::mcp::{
    auth::{
        auth_required_of, is_auth_required, AccessTokenSource, AuthFlow, AuthOutcome, AuthRequired,
        OauthConfig, PendingAuthorization, StoredAuth, StoredTokenSource, TokenSet, TokenStore,
    },
    jsonrpc_payload, parse_tool_def, McpHttpClient, McpToolDef,
};
// Renamed on the way through: bare `check`/`disconnect` would be far too
// generic at the core crate root, where they sit beside the engine's own API.
pub use entanglement_provider::mcp::auth::check as mcp_auth_check;
pub use entanglement_provider::mcp::auth::disconnect as mcp_auth_disconnect;
pub use protocol::{
    AgentMode, AgentProfile, AgentState, ApprovalScope, BashGrade, FileChangeKind, InMsg,
    McpAction, McpAuthAction, McpAuthStatus, McpServerSpec, McpServerStatus, OutEvent,
    PendingQuestion, Permission, PermissionProfile, ProfileDetail, Question, QuestionOption,
    Questions, SessionId, SessionInfo, ToolOverlayEntry,
};
