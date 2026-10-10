//! [`OutEvent`] accessors: the routing `session` and the content `seq`.

use crate::protocol::{OutEvent, SessionId};

impl OutEvent {
    /// The session this event belongs to, or `None` for a supervisor-global query
    /// reply that names no single session — [`SessionList`][OutEvent::SessionList]
    /// (#160), [`McpList`][OutEvent::McpList] and
    /// [`McpChanged`][OutEvent::McpChanged] (#375; MCP config is engine-global).
    /// [`History`][OutEvent::History] does name a session (the one whose
    /// history it carries), so it returns `Some`.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            OutEvent::SessionStarted { session, .. }
            | OutEvent::SessionEnded { session, .. }
            | OutEvent::SessionHibernated { session, .. }
            | OutEvent::History { session, .. }
            | OutEvent::Status { session, .. }
            | OutEvent::AgentChanged { session, .. }
            | OutEvent::ModeChanged { session, .. }
            | OutEvent::ModelChanged { session, .. }
            | OutEvent::GenerationChanged { session, .. }
            | OutEvent::SessionMetaChanged { session, .. }
            | OutEvent::ToolOverlayChanged { session, .. }
            | OutEvent::Plan { session, .. }
            | OutEvent::TextDelta { session, .. }
            | OutEvent::ReasoningDelta { session, .. }
            | OutEvent::ToolCallDelta { session, .. }
            | OutEvent::ToolCall { session, .. }
            | OutEvent::ToolRequest { session, .. }
            | OutEvent::ToolExec { session, .. }
            | OutEvent::UserQuestion { session, .. }
            | OutEvent::ToolOutput { session, .. }
            | OutEvent::TaskList { session, .. }
            | OutEvent::Usage { session, .. }
            | OutEvent::Error { session, .. }
            | OutEvent::Done { session, .. }
            | OutEvent::Compacted { session, .. }
            | OutEvent::FileChange { session, .. }
            | OutEvent::PlanChanged { session, .. }
            | OutEvent::SkillActive { session, .. }
            | OutEvent::AmbiguousRetry { session, .. }
            | OutEvent::SearchResult { session, .. }
            | OutEvent::ReasoningBlock { session, .. } => Some(session),
            OutEvent::SessionList { .. }
            | OutEvent::QuestionList { .. }
            | OutEvent::OperationList { .. }
            | OutEvent::PendingList { .. }
            | OutEvent::McpList { .. }
            | OutEvent::McpChanged { .. }
            | OutEvent::McpAuthChanged { .. }
            | OutEvent::Throttle { .. } => None,
        }
    }

    /// The monotonic per-session sequence number for a **content** event, or
    /// `None` for a point-in-time lifecycle/query event that carries no `seq`
    /// (`SessionStarted`, `SessionEnded`, `SessionList`, `QuestionList`,
    /// `OperationList`, `PendingList`, `History`, `Status`, `AgentChanged`, `ModelChanged`,
    /// `GenerationChanged`, `SessionMetaChanged`, `ModeChanged`). Returning `Option`
    /// instead of a fake `0`
    /// (#160, ADR-0072) lets a head tell "seq 0" apart from "no seq" — the
    /// supervisor-shed `Error` sentinel (seq `0`) is a real `Some(0)`, distinct
    /// from a lifecycle event's `None`.
    pub fn seq(&self) -> Option<u64> {
        match self {
            OutEvent::SessionStarted { .. }
            | OutEvent::SessionEnded { .. }
            | OutEvent::SessionHibernated { .. }
            | OutEvent::SessionList { .. }
            | OutEvent::QuestionList { .. }
            | OutEvent::OperationList { .. }
            | OutEvent::PendingList { .. }
            | OutEvent::McpList { .. }
            | OutEvent::McpChanged { .. }
            | OutEvent::McpAuthChanged { .. }
            | OutEvent::Throttle { .. }
            | OutEvent::History { .. }
            | OutEvent::Status { .. }
            | OutEvent::AgentChanged { .. }
            | OutEvent::ModeChanged { .. }
            | OutEvent::ModelChanged { .. }
            | OutEvent::GenerationChanged { .. }
            | OutEvent::SessionMetaChanged { .. }
            | OutEvent::ToolOverlayChanged { .. } => None,
            OutEvent::Plan { seq, .. }
            | OutEvent::TextDelta { seq, .. }
            | OutEvent::ReasoningDelta { seq, .. }
            | OutEvent::ToolCallDelta { seq, .. }
            | OutEvent::ToolCall { seq, .. }
            | OutEvent::ToolRequest { seq, .. }
            | OutEvent::ToolExec { seq, .. }
            | OutEvent::UserQuestion { seq, .. }
            | OutEvent::ToolOutput { seq, .. }
            | OutEvent::TaskList { seq, .. }
            | OutEvent::Usage { seq, .. }
            | OutEvent::Error { seq, .. }
            | OutEvent::Done { seq, .. }
            | OutEvent::Compacted { seq, .. }
            | OutEvent::FileChange { seq, .. }
            | OutEvent::PlanChanged { seq, .. }
            | OutEvent::SkillActive { seq, .. }
            | OutEvent::AmbiguousRetry { seq, .. }
            | OutEvent::SearchResult { seq, .. }
            | OutEvent::ReasoningBlock { seq, .. } => Some(*seq),
        }
    }
}
