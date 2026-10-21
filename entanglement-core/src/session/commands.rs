//! Handlers for the session loop's state-changing commands once they are
//! cleared to apply (deferral is the loop's call), plus the shared teardown.

use std::time::{SystemTime, UNIX_EPOCH};

use entanglement_provider::GenerationParams;
use tokio::sync::broadcast;

use super::{Session, TurnState};
use crate::holly::{ActivityRegistry, SeqRegistry};
use crate::protocol::{AgentState, OutEvent, SessionId, ToolOverlayEntry};
use crate::EngineConfig;

/// Wall-clock milliseconds since the Unix epoch, for lifecycle `ts` fields.
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `SetModel` (#218): re-resolve `(provider, model)` and rebind the backend,
/// or surface a recoverable `Error`.
pub(super) fn set_model(
    s: &mut Session,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    provider: String,
    model: String,
) {
    let Some(resolver) = cfg.model_resolver.as_ref() else {
        s.seq.emit(events, |seq| OutEvent::Error {
            session: session.clone(),
            seq,
            message: "model switching is not supported by this engine".to_string(),
        });
        return;
    };
    match resolver(s.user.as_ref(), &provider, &model) {
        Ok(resolved) => {
            s.rebind(session, resolved, events);
        }
        Err(e) => {
            s.seq.emit(events, |seq| OutEvent::Error {
                session: session.clone(),
                seq,
                message: format!("cannot switch model: {e}"),
            });
        }
    }
}

/// `SetGeneration` (#374, ADR-0094): merge partial overrides and ack with the
/// full effective params.
pub(super) fn set_generation(
    s: &mut Session,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    overrides: GenerationParams,
) {
    let mut merged = s.generation.unwrap_or_default();
    merged.apply_overrides(overrides);
    s.generation = Some(merged);
    // Recorded so a resumed session's replay-reconstructed live
    // override survives the session-start default re-application
    // (`EngineConfig::generation_resolver`) rather than being
    // silently overwritten by it (#374, ADR-0094).
    s.generation_by_agent.insert(s.agent.name.clone(), merged);
    let _ = events.send(OutEvent::GenerationChanged {
        session: session.clone(),
        generation: merged,
    });
}

/// `SetToolOverlay` (#539, ADR-0149): replace the overlay and ack with the
/// full effective list.
pub(super) fn set_tool_overlay(
    s: &mut Session,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    entries: Vec<ToolOverlayEntry>,
) {
    s.tool_overlay = entries.clone();
    let _ = events.send(OutEvent::ToolOverlayChanged {
        session: session.clone(),
        entries,
    });
}

/// `Stop`: cancel a parked turn by clearing its state (ADR-0017).
pub(super) fn stop(s: &mut Session, session: &SessionId, events: &broadcast::Sender<OutEvent>) {
    if s.turn.take().is_some() {
        // A cancelled turn is still a completed interaction — `Done`
        // is the resting state, not `Idle` (which stays reserved for
        // the genuinely-never-run-yet case at session start,
        // ADR-0139). `Stop` always cancels regardless of a pause
        // (#516, ADR-0208) — but doesn't lift one: pause and cancel
        // are orthogonal holds, so a still-paused session reports
        // `Paused`, not `Done`, until an explicit `ResumeSession`.
        let state = if s.paused {
            AgentState::Paused
        } else {
            AgentState::Done
        };
        let _ = events.send(OutEvent::Status {
            session: session.clone(),
            state,
        });
    }
}

/// `Pause` (#516, ADR-0208). Idempotent: no duplicate `Status` for an
/// already-paused session.
pub(super) fn pause(s: &mut Session, session: &SessionId, events: &broadcast::Sender<OutEvent>) {
    if !s.paused {
        s.paused = true;
        let _ = events.send(OutEvent::Status {
            session: session.clone(),
            state: AgentState::Paused,
        });
    }
}

/// `Unpause` (#516, ADR-0208): lift the hold. Returns `true` when a
/// drained-but-undriven parked batch should continue the turn now; otherwise
/// reports the state the session is resting in.
pub(super) fn unpause(
    s: &mut Session,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
) -> bool {
    if !s.paused {
        return false;
    }
    s.paused = false;
    if s.turn.as_ref().is_some_and(TurnState::is_drained) {
        return true;
    }
    let state = if s.turn.is_some() {
        AgentState::Working
    } else {
        AgentState::Done
    };
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state,
    });
    false
}

/// Retire `session`'s shared seq counter and drop its settledness entry,
/// returning the teardown timestamp. The counter stays behind in the registry
/// (#884, ADR-0239): detached work still running for this id (a background
/// script's card, ADR-0237) keeps minting seqs above every one already used,
/// and a resume continues above them. A new task for this id waits for this
/// one's exit before it registers (ADR-0244), so both removals are this
/// task's own entries.
pub(super) fn retire(session: &SessionId, seqs: &SeqRegistry, activity: &ActivityRegistry) -> u64 {
    let ts = now_ms();
    seqs.retire(session);
    activity
        .lock()
        .expect("activity registry mutex poisoned")
        .remove(session);
    ts
}
