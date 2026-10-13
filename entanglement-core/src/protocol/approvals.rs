//! Parked-approval snapshot shapes (#726): the payload of an
//! [`OutEvent::PendingList`] reply to [`InMsg::ListPending`].

use serde::{Deserialize, Serialize};

use crate::protocol::{ApprovalScope, SessionId};
#[cfg(doc)]
use crate::protocol::{InMsg, OutEvent};

/// What a parked approval is for — which runtime path force-parked it, and so
/// what an [`InMsg::Approve`] on it means. `ask_user` questions are not
/// approvals (they take [`InMsg::AnswerQuestion`]) and are listed by
/// [`InMsg::ListQuestions`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// A host/MCP tool call the session's permission mode graded `Ask`, or an
    /// out-of-root access that forced a prompt (ADR-0109).
    Tool,
    /// `propose_plan` (always parks; `Approve.mode` picks the target mode).
    Plan,
    /// `request_mode` (always parks; approving switches the session's mode).
    Mode,
    /// A `rhai` script binding whose own call graded `Ask` (tool
    /// `<name> (rhai)`), or a `rhai` **launch card** (tool exactly `rhai`,
    /// ADR-0228) — the one answered with `Approve.run_all`.
    Rhai,
}

/// One tool call parked awaiting [`InMsg::Approve`]/[`InMsg::Reject`], as
/// reported in an [`OutEvent::PendingList`] snapshot (#726). `tool`/`input`
/// are exactly what the still-open [`OutEvent::ToolRequest`] carried (so a
/// reconnecting head renders the same prompt it missed), plus the `session`
/// it belongs to — a `PendingList` can span many sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub session: SessionId,
    pub request_id: String,
    pub kind: ApprovalKind,
    pub tool: String,
    pub input: String,
    /// Human-readable why-it-parked, e.g. ``mode `build` grades `bash` Ask``.
    pub reason: String,
    /// The [`ApprovalScope`]s an `Approve` on this request actually honors —
    /// what a head should offer. Always starts with `Once`.
    pub scopes: Vec<ApprovalScope>,
    /// When the call parked, Unix epoch milliseconds.
    pub since_ms: u64,
}
