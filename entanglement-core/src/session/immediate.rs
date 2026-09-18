//! Commands that apply the moment they are dequeued — idle, parked, or
//! mid-stream alike — and so never enter the stash (ADR-0018).
//!
//! They are pure session state plus an ack that no in-flight round reads, so
//! deferring them protects nothing. And the stash only drains when a turn
//! ends: the narrator's `SetSessionMeta`, one per tool call, used to pile up
//! behind a long streamed turn until the stash hit its cap and the user's own
//! next `Prompt` was the command refused.

use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;

use tokio::sync::broadcast;

use super::mode::apply_set_mode;
use super::{cap_meta_field, stash_or_reject, SessionCmd};
use crate::protocol::{OutEvent, SessionId};

/// Apply `cmd` now if it is an apply-immediately command; otherwise hand it
/// back. Takes `Session`'s fields rather than `&mut Session` because the
/// mid-stream caller holds `s.llm` borrowed.
pub(super) fn try_apply(
    cmd: SessionCmd,
    s: Fields<'_>,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
) -> Result<(), SessionCmd> {
    match cmd {
        SessionCmd::SetMode(mode) => {
            apply_set_mode(s.mode, s.mode_transition_from, mode, session, events);
        }
        SessionCmd::SetSessionMeta(name, action, if_unset) => {
            // `None` leaves a field untouched; `Some("")` clears it.
            // `if_unset` (#553, the auto-title generator's path): a session
            // that already has a name — set via `/name`, or restored on resume
            // before this command was ever sent — keeps it; the generator's
            // write silently no-ops instead of racing a user-set name.
            if let Some(name) = name {
                let name = cap_meta_field(name);
                if !if_unset || s.name.is_none() {
                    *s.name = (!name.is_empty()).then_some(name);
                }
            }
            if let Some(action) = action {
                let action = cap_meta_field(action);
                *s.action = (!action.is_empty()).then_some(action);
            }
            let _ = events.send(OutEvent::SessionMetaChanged {
                session: session.clone(),
                name: s.name.clone(),
                action: s.action.clone(),
            });
        }
        // Lineage mirror: idempotent — a duplicate spawn or an unknown close
        // is a no-op.
        SessionCmd::ChildSpawned(child) => {
            if !s.children.contains(&child) {
                s.children.push(child);
            }
        }
        SessionCmd::ChildClosed(child) => s.children.retain(|c| c != &child),
        other => return Err(other),
    }
    Ok(())
}

/// The fields [`try_apply`] writes, borrowed disjointly from `Session`.
pub(super) struct Fields<'a> {
    pub mode: &'a mut String,
    pub mode_transition_from: &'a mut Option<String>,
    pub name: &'a mut Option<String>,
    pub action: &'a mut Option<String>,
    pub children: &'a mut Vec<SessionId>,
}

macro_rules! fields {
    ($s:expr) => {
        $crate::session::immediate::Fields {
            mode: &mut $s.mode,
            mode_transition_from: &mut $s.mode_transition_from,
            name: &mut $s.name,
            action: &mut $s.action,
            children: &mut $s.children,
        }
    };
}
pub(super) use fields;

/// A command arriving while a round streams: applied now if it can be,
/// otherwise stashed for after the turn. The user-issued, deferrable commands
/// go through the same capped [`stash_or_reject`] the session loop uses;
/// lifecycle commands (`Hibernate`, `Pause`/`Unpause`, `ToolResult`) are
/// never dropped.
pub(super) fn apply_or_stash_mid_stream(
    cmd: SessionCmd,
    s: Fields<'_>,
    stash: &mut VecDeque<SessionCmd>,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    seq: &AtomicU64,
) {
    let Err(cmd) = try_apply(cmd, s, session, events) else {
        return;
    };
    tracing::debug!(cmd = ?cmd, "command arrived mid-stream; stashed for replay after turn");
    match cmd {
        SessionCmd::Prompt(_)
        | SessionCmd::SetModel(..)
        | SessionCmd::SetGeneration(_)
        | SessionCmd::SetToolOverlay(_)
        | SessionCmd::Oneshot(..) => stash_or_reject(stash, cmd, session, events, seq),
        other => stash.push_back(other),
    }
}
