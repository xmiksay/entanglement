//! Shared seam plumbing for the runtime-owned tool orchestrators (#205).
//!
//! Every runtime-owned tool — the generic `tool_runner` dispatch plus
//! `ask_user`, `propose_plan`, `rhai`, `agent_spawn`/`agent`, `agent_poll` —
//! speaks the same #58 round-trip: it emits a request event, parks on the
//! engine's inbound fan-out for the head's decision, and folds the outcome back
//! as an [`InMsg::ToolResult`]. Two steps of that round-trip were copied across
//! every module:
//!
//! - **[`reply`]** — the `ToolResult` fold-back (was duplicated 6×);
//! - **[`await_decision`]** — the park loop that filters the head's answer to
//!   the waiting `(session, request_id)` and treats the ADR-0017
//!   Stop/Lagged/Closed cases identically (was duplicated across `tool_runner`,
//!   `propose_plan`, `ask_user`, and `script`).
//!
//! Centralizing both here means a park-loop fix (a new terminal message, a
//! changed Lagged policy) propagates to every call site instead of drifting.

use entanglement_core::{ApprovalScope, ContentPart, Holly, InMsg, SessionId};
use tokio::sync::broadcast::{error::RecvError, Receiver};

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
    let _ = holly
        .send(InMsg::ToolResult {
            session,
            request_id,
            content,
        })
        .await;
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

/// Park on the engine's inbound fan-out until the head answers the pending
/// request for `(session, request_id)`, then return the [`Decision`]. Messages
/// for other sessions/requests are ignored; a `Lagged` gap is skipped; a `Stop`
/// for the session or a closed inbox both resolve to [`Decision::Stop`].
///
/// The caller emits its request event (`ToolRequest`/`UserQuestion`) and sets
/// the waiting status *before* calling this — subscribing to the inbound
/// fan-out first (the callers do so synchronously) so a fast answer can't race
/// ahead of the park.
pub async fn await_decision(
    inbound: &mut Receiver<InMsg>,
    session: &SessionId,
    request_id: &str,
) -> Decision {
    loop {
        match inbound.recv().await {
            Ok(InMsg::Approve {
                session: s,
                request_id: rid,
                scope,
            }) if &s == session && rid == request_id => return Decision::Approve { scope },
            Ok(InMsg::Reject {
                session: s,
                request_id: rid,
                reason,
            }) if &s == session && rid == request_id => return Decision::Reject { reason },
            Ok(InMsg::AnswerQuestion {
                session: s,
                request_id: rid,
                answer,
            }) if &s == session && rid == request_id => return Decision::Answer { answer },
            Ok(InMsg::Stop { session: s }) if &s == session => return Decision::Stop,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return Decision::Stop,
        }
    }
}
