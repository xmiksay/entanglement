//! Routing helpers for the supervisor loop: per-session command delivery with
//! backpressure shedding (ADR-0028), supervisor-level error emission, resumed
//! session metadata, and the [`InMsg`] → [`SessionCmd`] mapping. Split out of
//! the supervisor loop so the routing seam reads on its own.

use tokio::sync::{broadcast, mpsc};

use crate::protocol::{InMsg, OutEvent, SessionId, SessionInfo};
use crate::session::SessionCmd;

use super::{DEFAULT_PROFILE, ROUTE_ATTEMPTS};

/// Route a command to a session without letting one saturated session block the
/// supervisor's single loop — and thereby delay routing to *every* other
/// session (ADR-0028). Tries a non-blocking send first; on a full channel it
/// retries a bounded number of times, yielding between attempts so a
/// merely-behind session can drain, then sheds the command with an
/// [`OutEvent::Error`] rather than parking the supervisor. A closed channel
/// (session already gone) is dropped silently.
pub(super) async fn route_to_session(
    tx: &mpsc::Sender<SessionCmd>,
    cmd: SessionCmd,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
) {
    use mpsc::error::TrySendError;
    let mut cmd = cmd;
    for _ in 0..ROUTE_ATTEMPTS {
        match tx.try_send(cmd) {
            Ok(()) => return,
            Err(TrySendError::Closed(_)) => return,
            Err(TrySendError::Full(returned)) => {
                cmd = returned;
                tokio::task::yield_now().await;
            }
        }
    }
    tracing::warn!(%session, "session command channel saturated; command shed");
    emit_supervisor_error(
        events,
        session,
        "session busy: command dropped (command channel saturated)",
    );
}

/// Emit a supervisor-level [`OutEvent::Error`] for a session the supervisor
/// rejects or sheds (a refused resurrection, a failed replay, a saturated
/// channel). `seq` is `0` because the supervisor can't mint the session's
/// monotonic seq — the session task owns it. On these exceptional paths the
/// tracing log is the primary signal; the event tells any listening head the
/// message did not land, rather than letting it vanish silently.
pub(super) fn emit_supervisor_error(
    events: &broadcast::Sender<OutEvent>,
    session: &SessionId,
    message: &str,
) {
    let _ = events.send(OutEvent::Error {
        session: session.clone(),
        seq: 0,
        message: message.to_string(),
    });
}

/// Best-effort [`SessionInfo`] for a resumed session, read from the first
/// `SessionStarted` record in its replay log. Absent (an older log), it's
/// treated as a root under the base `build` profile.
pub(super) fn resume_meta(
    session: &SessionId,
    records: &[(Option<InMsg>, OutEvent)],
) -> SessionInfo {
    for (_, ev) in records {
        if let OutEvent::SessionStarted {
            parent,
            profile,
            root,
            ..
        } = ev
        {
            return SessionInfo {
                session: session.clone(),
                parent: parent.clone(),
                profile: profile.clone(),
                root: *root,
            };
        }
    }
    SessionInfo {
        session: session.clone(),
        parent: None,
        profile: DEFAULT_PROFILE.to_string(),
        root: true,
    }
}

pub(super) fn msg_to_cmd(msg: InMsg) -> SessionCmd {
    match msg {
        InMsg::Prompt { text, .. } => SessionCmd::Prompt(text),
        InMsg::ToolResult {
            request_id, output, ..
        } => SessionCmd::ToolResult(request_id, output),
        InMsg::Stop { .. } => SessionCmd::Stop,
        InMsg::SetPlan { content, .. } => SessionCmd::SetPlan(content),
        InMsg::SetTasks { content, .. } => SessionCmd::SetTasks(content),
        InMsg::SetAgent { agent, .. } => SessionCmd::SetAgent(agent),
        // Approve/Reject/AnswerQuestion and the ListSessions/CloseSession
        // lifecycle queries are filtered out before routing (see supervisor);
        // Resume and Spawn are handled specially. None reach here.
        InMsg::Approve { .. }
        | InMsg::Reject { .. }
        | InMsg::AnswerQuestion { .. }
        | InMsg::ListSessions { .. }
        | InMsg::CloseSession { .. }
        | InMsg::Resume { .. }
        | InMsg::Spawn { .. } => {
            unreachable!("Approve/Reject/AnswerQuestion/ListSessions/CloseSession/Resume/Spawn are not routed to sessions")
        }
    }
}
