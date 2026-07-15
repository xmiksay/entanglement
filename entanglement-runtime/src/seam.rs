//! Shared seam plumbing for the runtime-owned tool orchestrators (#205).
//!
//! Every runtime-owned tool — the generic `tool_runner` dispatch plus
//! `ask_user`, `propose_plan`, `rhai`, `agent_spawn`/`agent`, `agent_poll` —
//! speaks the same #58 round-trip: it emits a request event, parks for the
//! head's decision, and folds the outcome back as an [`InMsg::ToolResult`]. Two
//! steps of that round-trip were copied across every module:
//!
//! - **[`reply`]** — the `ToolResult` fold-back (was duplicated 6×);
//! - the **park for a decision** — previously each orchestrator held its own
//!   `broadcast` subscription of the inbound fan-out and filtered it to
//!   `(session, request_id)`. That per-task subscriber could *lag* under burst
//!   and silently drop the very `Approve`/`Reject`/`AnswerQuestion` it waited for,
//!   parking the request forever (#156). Decision delivery now runs off the
//!   lag-proof [`crate::pending::PendingDecisions`] registry: a single light
//!   router task ([`crate::tool_runner`]) consumes the fan-out and fans each
//!   decision — mapped here by [`Decision::from_inmsg`] — to its parked waiter's
//!   oneshot. This module keeps the [`Decision`] type and the fold-back.

use entanglement_core::{ApprovalScope, ContentPart, Holly, InMsg, SessionId};
// `ToolResult` folds back over the privileged in-process handle (#155), not the
// untrusted wire path — a forged `ToolResult` off a wire head must never resolve
// a parked turn.

/// Fold a **text** tool result back to core as the #58 `ToolResult`, completing
/// the parked `ToolExec` round-trip. The text-producing tools (denials, refusals,
/// `rhai`, `ask_user`, …) all end here; an empty string folds to no content
/// parts, matching a text-only tool message.
pub async fn reply(holly: &Holly, session: SessionId, request_id: String, output: String) {
    let content = if output.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(output)]
    };
    reply_content(holly, session, request_id, content).await;
}

/// Fold a **multimodal** tool result back to core (#221) — `read` uses this to
/// return an image content block. Other callers use the text [`reply`].
pub async fn reply_content(
    holly: &Holly,
    session: SessionId,
    request_id: String,
    content: Vec<ContentPart>,
) {
    let _ = holly.submit_tool_result(session, request_id, content).await;
}

/// The head's answer to a parked tool round-trip, already filtered to the
/// waiting `(session, request_id)`.
pub enum Decision {
    /// `Approve` — carries the approval scope (#174).
    Approve { scope: ApprovalScope },
    /// `Reject` — carries the optional reason.
    Reject { reason: Option<String> },
    /// `AnswerQuestion` — the picked label or typed text (`ask_user`, #90).
    Answer { answer: String },
    /// `Stop` for this session, or the inbound stream closed: unwind silently —
    /// core cancels the turn on the same `Stop`, so no `ToolResult` is owed
    /// (ADR-0017).
    Stop,
}

impl Decision {
    /// Map an inbound frame to its routing tuple `(session, request_id,
    /// Decision)`, or `None` for a frame that is not a head decision (#156). The
    /// executor's single inbound router uses this to fan each `Approve`/`Reject`/
    /// `AnswerQuestion` to its parked waiter in
    /// [`crate::pending::PendingDecisions`]. `Stop` is *not* mapped here — it is
    /// session-scoped, not request-scoped, so the router unwinds every waiter of
    /// the session via [`crate::pending::PendingDecisions::stop_session`].
    pub fn from_inmsg(msg: InMsg) -> Option<(SessionId, String, Decision)> {
        match msg {
            InMsg::Approve {
                session,
                request_id,
                scope,
            } => Some((session, request_id, Decision::Approve { scope })),
            InMsg::Reject {
                session,
                request_id,
                reason,
            } => Some((session, request_id, Decision::Reject { reason })),
            InMsg::AnswerQuestion {
                session,
                request_id,
                answer,
            } => Some((session, request_id, Decision::Answer { answer })),
            _ => None,
        }
    }
}
