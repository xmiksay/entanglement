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
pub mod id_gen;
pub mod protocol;
pub mod session;

pub use context::Context;
pub use holly::{
    AgentCatalog, ConfigError, EngineConfig, FileReverter, Holly, PersistenceSignal, SessionLoader,
    SessionModel, SystemPromptResolver, ToolSpecResolver, UnknownSession, WireError,
    WorkdirResolver, DEFAULT_AGENT, DEFAULT_MODE,
};
pub use id_gen::{DefaultIdGen, IdContext, IdGen, IdKind};
// The LLM seam (trait + DTOs + wire `Message`) lives in `entanglement-provider`,
// the leaf crate; core depends on it and re-exports for its heads (ADR-0053).
// The `mcp` envelope's reserved name rides the same path (ADR-0226): the
// runtime advertises the envelope and core unwraps it, so both need one
// spelling.
pub use entanglement_provider::{
    call_endpoint, content_text, mcp_tool_name, stream_from_response, AuxLlmResolver, Catalog,
    ContentPart, DebugFlags, DummyLlm, EchoLlm, EndpointMethod, EndpointResponse, GenerationParams,
    GenerationResolver, HttpClient, ImageSource, Llm, LlmEvent, LlmFactory, LlmRequest,
    LlmResponse, LlmStream, Message, MessageRole, ModelEntry, ModelPricing, ModelResolver,
    ProviderMcpServer, ReasoningEffort, ResolvedModel, RetryConfig, SharedStateLocation,
    StopReason, ToolCall, ToolSpec, Usage, UserId, WebSearchConfig, Wire, DEBUG_ENV,
    ENDPOINT_RESPONSE_CAP, MCP_TOOL,
};
// The MCP client mechanism — transport + OAuth (ADR-0153) — also lives in the
// leaf crate. Core carries no MCP *logic* (ADR-0067): this is a pass-through so
// the runtime reaches these types on the same path `ProviderMcpServer` already
// takes, including in the lean build that names no provider dependency of its
// own. Nothing in core calls them. `HttpClient` above (#559) is what the MCP
// HTTP transport is handed so its traffic shares the same per-endpoint
// pool/RPM/concurrency/retry as LLM traffic — not `McpHttpClient`'s own name.
pub use entanglement_provider::mcp::{
    is_response, jsonrpc_payload, list_tools_paginated, parse_tool_def, server_request_reply,
    McpHttpClient, McpToolDef,
};
pub use entanglement_provider::oauth::{
    auth_required_of, is_auth_required, AccessTokenSource, AuthFlow, AuthOutcome, AuthRequired,
    DeviceFlow, OauthConfig, PendingAuthorization, PendingDeviceAuthorization, StoredAuth,
    StoredTokenSource, TokenSet, TokenStore,
};
// Renamed on the way through: bare `check`/`disconnect` would be far too
// generic at the core crate root, where they sit beside the engine's own API.
// The SSRF-guarded `web_fetch` mechanism (#736, ADR-0220) — same
// pass-through as the MCP transport above: the runtime's tool reaches it here
// in the lean build too, and never names `reqwest`.
pub use entanglement_provider::oauth::check as mcp_auth_check;
pub use entanglement_provider::oauth::disconnect as mcp_auth_disconnect;
pub use entanglement_provider::web_fetch;
pub use protocol::{
    Agent, AgentState, ApprovalKind, ApprovalScope, CompactionMode, FileChangeKind,
    FileRevertReport, InMsg, McpAction, McpAuthAction, McpAuthStatus, McpServerSpec,
    McpServerStatus, ModelChoice, OperationInfo, OperationKind, OperationStatus, OutEvent,
    PendingApproval, PendingQuestion, Permission, PermissionProfile, Question, QuestionOption,
    Questions, RevertSkip, SessionId, SessionInfo, ToolEnvelope, ToolOverlayEntry, UsagePurpose,
    RETIRED_IN_MSG_KINDS, RETIRED_OUT_EVENT_KINDS,
};
