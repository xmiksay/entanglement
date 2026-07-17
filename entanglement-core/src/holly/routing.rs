//! Routing helpers for the supervisor loop: per-session command delivery with
//! backpressure shedding (ADR-0028), supervisor-level error emission, resumed
//! session metadata, and the [`InMsg`] → [`SessionCmd`] mapping. Split out of
//! the supervisor loop so the routing seam reads on its own.

use tokio::sync::{broadcast, mpsc};

use crate::protocol::{InMsg, OutEvent, SessionId, SessionInfo};
use crate::session::SessionCmd;

use super::{next_seq_for, SeqRegistry, DEFAULT_PROFILE, ROUTE_ATTEMPTS};

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
    seqs: &SeqRegistry,
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
        seqs,
        session,
        "session busy: command dropped (command channel saturated)",
    );
}

/// Emit a supervisor-level [`OutEvent::Error`] for a session the supervisor
/// rejects or sheds (a refused resurrection, a failed replay, a saturated
/// channel). When the target session is *live* (e.g. a saturated channel) it has
/// a registered seq counter, so the error mints a real monotonic seq from the
/// shared registry (#157) and takes its ordered place in that session's content
/// stream. For an id with no live session (a refused resume/spawn of a
/// closed/unknown id) there is no counter, so the seq is `0` — a value core never
/// mints, so it can't collide with content, and heads render it unconditionally
/// (the seq-`0` bypass) instead of dropping it under a `seq > last` dedupe. On
/// these exceptional paths the tracing log is the primary signal; the
/// event tells any listening head the message did not land.
pub(super) fn emit_supervisor_error(
    events: &broadcast::Sender<OutEvent>,
    seqs: &SeqRegistry,
    session: &SessionId,
    message: &str,
) {
    let _ = events.send(OutEvent::Error {
        session: session.clone(),
        seq: next_seq_for(seqs, session),
        message: message.to_string(),
    });
}

/// Best-effort [`SessionInfo`] for a resumed session, read from `session`'s own
/// `SessionStarted` record in the (possibly whole-sub-tree) replay log — never
/// another session's, since a root's log interleaves every spawned child's
/// records too (#415, mirroring the `is_target` scoping in
/// [`Session::replay`][crate::session::Session::replay]). Absent (an older log,
/// or `session` never appears in it), it's treated as a root under the base
/// `build` profile.
pub(super) fn resume_meta(
    session: &SessionId,
    records: &[(Option<InMsg>, OutEvent)],
) -> SessionInfo {
    for (_, ev) in records {
        if let OutEvent::SessionStarted {
            session: started,
            parent,
            profile,
            root,
            ..
        } = ev
        {
            if started != session {
                continue;
            }
            return SessionInfo {
                session: session.clone(),
                parent: parent.clone(),
                profile: profile.clone(),
                root: *root,
                // The replay log carries only the profile *name*; the caller fills
                // the resolved detail from the replayed session's profile (#189).
                profile_detail: None,
            };
        }
    }
    SessionInfo {
        session: session.clone(),
        parent: None,
        profile: DEFAULT_PROFILE.to_string(),
        root: true,
        profile_detail: None,
    }
}

/// Map a session-scoped [`InMsg`] to the [`SessionCmd`] routed to its session
/// task, or `None` for a variant that is never routed (handled specially or
/// consumed off the inbound fan-out by the supervisor / a runtime service).
///
/// Returns `Option` rather than panicking on the non-routable variants (#160):
/// the supervisor filters them out before calling this, so a `None` here is a
/// contract slip to log-and-drop, not a reason to crash every live session (the
/// former `unreachable!` took down the whole supervisor loop).
pub(super) fn msg_to_cmd(msg: InMsg) -> Option<SessionCmd> {
    Some(match msg {
        InMsg::Prompt { content, .. } => SessionCmd::Prompt(content),
        InMsg::ToolResult {
            request_id,
            content,
            ..
        } => SessionCmd::ToolResult(request_id, content),
        InMsg::Stop { .. } => SessionCmd::Stop,
        InMsg::SetAgent { agent, .. } => SessionCmd::SetAgent(agent),
        InMsg::SetModel {
            provider, model, ..
        } => SessionCmd::SetModel(provider, model),
        InMsg::SetGeneration { overrides, .. } => SessionCmd::SetGeneration(overrides),
        InMsg::Oneshot { op, args, .. } => SessionCmd::Oneshot(op, args),
        // Approve/Reject/AnswerQuestion and the ListSessions/ReplayFrom/
        // CloseSession/HibernateSession queries are filtered out before routing
        // (see supervisor); Resume and Spawn are handled specially. The MCP ops
        // (#375) are engine-global like ListSessions — a runtime service answers
        // them off the inbound fan-out, never a session task. None reach here.
        InMsg::Approve { .. }
        | InMsg::Reject { .. }
        | InMsg::AnswerQuestion { .. }
        | InMsg::ListSessions { .. }
        | InMsg::McpList { .. }
        | InMsg::McpAdd { .. }
        | InMsg::McpRemove { .. }
        | InMsg::ReplayFrom { .. }
        | InMsg::CloseSession { .. }
        | InMsg::HibernateSession { .. }
        | InMsg::Resume { .. }
        | InMsg::Spawn { .. } => return None,
    })
}
