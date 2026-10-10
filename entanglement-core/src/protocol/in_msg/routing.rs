//! [`InMsg`] routing (`session`) and trust classification (`wire_allowed`).

use crate::protocol::{InMsg, SessionId};

impl InMsg {
    /// The session this message targets, or `None` for a supervisor-global query
    /// that names no session — [`ListSessions`][InMsg::ListSessions],
    /// [`ListQuestions`][InMsg::ListQuestions] (#515) and
    /// [`ListOperations`][InMsg::ListOperations] (#607; both carry an optional
    /// `session` that is a result *filter*, not a routing target — a `None`
    /// filter spans every session), and the MCP
    /// ops [`McpList`][InMsg::McpList]/[`McpAdd`][InMsg::McpAdd]/
    /// [`McpRemove`][InMsg::McpRemove] (#375; MCP config is engine-global, not
    /// per-session). Every other
    /// variant, including the session-scoped [`ReplayFrom`][InMsg::ReplayFrom]
    /// query and [`RetractQuestion`][InMsg::RetractQuestion]/
    /// [`ReplaceQuestion`][InMsg::ReplaceQuestion] (#515, resolved by the
    /// runtime off the inbound fan-out exactly like `AnswerQuestion`, never
    /// routed to a session task — see `msg_to_cmd`), carries one.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            InMsg::Prompt { session, .. }
            | InMsg::Approve { session, .. }
            | InMsg::Reject { session, .. }
            | InMsg::ToolResult { session, .. }
            | InMsg::AnswerQuestion { session, .. }
            | InMsg::RetractQuestion { session, .. }
            | InMsg::ReplaceQuestion { session, .. }
            | InMsg::Stop { session }
            | InMsg::PauseSession { session }
            | InMsg::ResumeSession { session }
            | InMsg::ReplayFrom { session, .. }
            | InMsg::CloseSession { session }
            | InMsg::HibernateSession { session }
            | InMsg::SetMode { session, .. }
            | InMsg::SetModel { session, .. }
            | InMsg::SetGeneration { session, .. }
            | InMsg::SetSessionMeta { session, .. }
            | InMsg::SetToolOverlay { session, .. }
            | InMsg::Oneshot { session, .. }
            | InMsg::Spawn { session, .. }
            | InMsg::Resume { session, .. } => Some(session),
            InMsg::ListSessions { .. }
            | InMsg::ListQuestions { .. }
            | InMsg::ListOperations { .. }
            | InMsg::McpList { .. }
            | InMsg::McpAdd { .. }
            | InMsg::McpRemove { .. }
            | InMsg::McpAuth { .. } => None,
        }
    }

    /// Whether this variant may originate from an **untrusted wire head** (#155).
    ///
    /// The trusted/untrusted frame split. A head deserializing attacker-adjacent
    /// bytes (stdio `pipe`, WebSocket `serve`) forwards only the allowlisted
    /// frames below. Everything else is **runtime-authored in process** (or an
    /// engine-privileged control), never wire-forgeable:
    ///
    /// - [`ToolResult`][InMsg::ToolResult] resolves a parked turn matched on
    ///   `request_id` alone — a forged one bypasses execution *and* permission;
    /// - [`Spawn`][InMsg::Spawn] mints a child session bypassing the tool path's
    ///   `spawn_refusal` gate;
    /// - [`Resume`][InMsg::Resume] is internal (`#[serde(skip)]`, never on wire);
    /// - [`HibernateSession`][InMsg::HibernateSession] is an embedder memory-eviction
    ///   control (#318) — a wire head must not be able to evict another session's
    ///   in-memory state;
    /// - [`McpAdd`][InMsg::McpAdd]/[`McpRemove`][InMsg::McpRemove] (#472,
    ///   ADR-0124, reversing #375's wire tier): an unapproved `McpAdd` spawns an
    ///   arbitrary local subprocess, and the `serve` head's origin gate is
    ///   opt-in — a hostile web page could otherwise drive it cross-origin over
    ///   the local WebSocket. ADR-0047's "config is consent" covers the *config
    ///   file*, not an unauthenticated wire frame. Trusted heads (the TUI
    ///   `/mcp` command) keep both via [`Holly::send`][crate::Holly::send];
    ///   `McpList` is read-only and stays wire-allowed.
    /// - [`SetToolOverlay`][InMsg::SetToolOverlay] (#539, ADR-0149, same
    ///   rationale as `McpAdd`/`McpRemove`): an **enable** entry injects tools
    ///   past the agent mask — including, per ADR-0163 (#611), live-enabling a
    ///   lazily-registrable built-in like `bash` — optionally graded `allow`
    ///   with no approval prompt, so it stays trusted-only. A **deny-only**
    ///   overlay (every entry has `deny: true`) only *withdraws* tools the
    ///   profile already advertises — it can never widen what the model may
    ///   call — so it is wire-allowed (#634, ADR-0149 "Consequences" amended
    ///   by ADR-0177). An empty `entries` list (clearing back to the profile
    ///   default) is vacuously deny-only and also wire-allowed.
    /// - [`SetMode`][InMsg::SetMode] (ADR-0207): a mode *is* the session's
    ///   authority — unlike the agent, which is identity fixed at session
    ///   start and carries no live switch message at all (ADR-0207 §9). A
    ///   wire-forged `SetMode` would let an unauthenticated head widen its own
    ///   permission posture (e.g. `research` → `build`), so it is
    ///   trusted-only; the analogous widening the model itself may request is
    ///   the graded `request_mode` tool, not this frame.
    ///
    /// [`RetractQuestion`][InMsg::RetractQuestion]/[`ReplaceQuestion`][InMsg::ReplaceQuestion]
    /// and [`ListQuestions`][InMsg::ListQuestions] (#515) are wire-allowed: the
    /// former two withdraw/revise a question the *same* head already saw
    /// surfaced (no more privileged than answering it, which is already
    /// wire-allowed), and the latter is a read-only snapshot query, exactly
    /// like `ListSessions`/`McpList`. [`ListOperations`][InMsg::ListOperations]
    /// (#607, ADR-0161 §6) is wire-allowed for the same reason: it only reads
    /// the caller's own outstanding background jobs/sub-agents, mutating
    /// nothing.
    ///
    /// The executor submits `ToolResult`/`Spawn` over the privileged in-process
    /// [`Holly::send`][crate::Holly::send] (it holds the handle); a wire head uses
    /// [`Holly::send_from_wire`][crate::Holly::send_from_wire], which enforces this
    /// allowlist.
    ///
    /// An explicit allow-list `match` — not a negated blocklist — so a **new**
    /// variant fails closed: it is wire-refused until someone adds it here
    /// deliberately, and the exhaustive match makes that decision a compile
    /// error to skip (mirroring [`session`][Self::session] /
    /// [`variant_name`][Self::variant_name]).
    pub fn wire_allowed(&self) -> bool {
        match self {
            InMsg::Prompt { .. }
            | InMsg::Approve { .. }
            | InMsg::Reject { .. }
            | InMsg::AnswerQuestion { .. }
            | InMsg::RetractQuestion { .. }
            | InMsg::ReplaceQuestion { .. }
            | InMsg::Stop { .. }
            | InMsg::PauseSession { .. }
            | InMsg::ResumeSession { .. }
            | InMsg::ListSessions { .. }
            | InMsg::ListQuestions { .. }
            | InMsg::ListOperations { .. }
            | InMsg::McpList { .. }
            | InMsg::ReplayFrom { .. }
            | InMsg::CloseSession { .. }
            | InMsg::SetModel { .. }
            | InMsg::SetGeneration { .. }
            | InMsg::SetSessionMeta { .. }
            | InMsg::Oneshot { .. } => true,
            // Deny-only (#634, ADR-0177): withdraws tools, never grants one —
            // safe over an unauthenticated wire. Any enable entry refuses the
            // whole frame (full replacement, so a mixed list is refused
            // outright rather than silently dropping the enable entries).
            InMsg::SetToolOverlay { entries, .. } => entries.iter().all(|e| e.deny),
            InMsg::ToolResult { .. }
            | InMsg::Spawn { .. }
            | InMsg::Resume { .. }
            | InMsg::HibernateSession { .. }
            | InMsg::McpAdd { .. }
            | InMsg::McpRemove { .. }
            | InMsg::McpAuth { .. }
            | InMsg::SetMode { .. } => false,
        }
    }

    /// The serde `kind` tag of this variant, for diagnostics (e.g. a rejected
    /// wire frame). Matches the `snake_case` wire discriminant.
    pub fn variant_name(&self) -> &'static str {
        match self {
            InMsg::Prompt { .. } => "prompt",
            InMsg::Approve { .. } => "approve",
            InMsg::Reject { .. } => "reject",
            InMsg::ToolResult { .. } => "tool_result",
            InMsg::AnswerQuestion { .. } => "answer_question",
            InMsg::RetractQuestion { .. } => "retract_question",
            InMsg::ReplaceQuestion { .. } => "replace_question",
            InMsg::Stop { .. } => "stop",
            InMsg::PauseSession { .. } => "pause_session",
            InMsg::ResumeSession { .. } => "resume_session",
            InMsg::ListSessions { .. } => "list_sessions",
            InMsg::ListQuestions { .. } => "list_questions",
            InMsg::ListOperations { .. } => "list_operations",
            InMsg::McpList { .. } => "mcp_list",
            InMsg::McpAdd { .. } => "mcp_add",
            InMsg::McpRemove { .. } => "mcp_remove",
            InMsg::McpAuth { .. } => "mcp_auth",
            InMsg::ReplayFrom { .. } => "replay_from",
            InMsg::CloseSession { .. } => "close_session",
            InMsg::HibernateSession { .. } => "hibernate_session",
            InMsg::SetMode { .. } => "set_mode",
            InMsg::SetModel { .. } => "set_model",
            InMsg::SetGeneration { .. } => "set_generation",
            InMsg::SetSessionMeta { .. } => "set_session_meta",
            InMsg::SetToolOverlay { .. } => "set_tool_overlay",
            InMsg::Oneshot { .. } => "oneshot",
            InMsg::Spawn { .. } => "spawn",
            InMsg::Resume { .. } => "resume",
        }
    }
}
