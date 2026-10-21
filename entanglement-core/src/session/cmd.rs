//! The supervisor→session command set and the capped deferred-command stash.

use std::collections::VecDeque;

use entanglement_provider::ContentPart;
use tokio::sync::broadcast;

#[cfg(doc)]
use super::Session;
use crate::holly::SeqCell;
#[cfg(doc)]
use crate::protocol::InMsg;
use crate::protocol::{OutEvent, SessionId, ToolOverlayEntry};

/// Ceiling on the deferred-command queue (#556): while paused (or mid-turn),
/// every wire-allowed command that arrives is stashed until the hold lifts or
/// the live turn ends — a stuck automation retrying against a paused/busy
/// session must not grow this without bound.
const MAX_STASHED_COMMANDS: usize = 64;

/// Push `cmd` onto `stash` unless it's already at [`MAX_STASHED_COMMANDS`], in
/// which case `cmd` is dropped and the caller told via `OutEvent::Error`
/// rather than the queue growing forever.
pub(super) fn stash_or_reject(
    stash: &mut VecDeque<SessionCmd>,
    cmd: SessionCmd,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    seq: &SeqCell,
) {
    if stash.len() >= MAX_STASHED_COMMANDS {
        seq.emit(events, |seq| OutEvent::Error {
            session: session.clone(),
            seq,
            message: format!(
                "too many commands queued while this session is busy/paused \
                 (cap {MAX_STASHED_COMMANDS}) — dropped"
            ),
        });
        return;
    }
    stash.push_back(cmd);
}

/// Ceiling on `SetSessionMeta`'s `name`/`action` (#556): both are persisted
/// and re-broadcast on every set, so an unbounded value bloats the event log
/// and every session listing one write at a time.
const MAX_SESSION_META_LEN: usize = 200;

/// Byte-cap `s` at [`MAX_SESSION_META_LEN`], backing off to the nearest UTF-8
/// char boundary rather than panicking mid-character.
pub(super) fn cap_meta_field(s: String) -> String {
    if s.len() <= MAX_SESSION_META_LEN {
        return s;
    }
    let mut cut = MAX_SESSION_META_LEN;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// Commands routed to a single session by the supervisor (InMsg minus session id).
#[derive(Debug, Clone)]
pub(crate) enum SessionCmd {
    Prompt(Vec<ContentPart>),
    /// Output of a runtime-executed tool (`request_id`, multimodal `content`,
    /// `is_error`, `duration_ms`) — resolves a pending [`OutEvent::ToolExec`]
    /// round-trip (#58). `content` is text today, an image block when `read`
    /// opens an image (#221). `is_error`/`duration_ms` (#636, ADR-0176) and
    /// `exit_code` (#681, ADR-0186) ride straight through to
    /// [`OutEvent::ToolOutput`] — the structured side channel is display-only,
    /// so it does not feed `Context` (the model still only sees `content`'s
    /// text). Approval (`Approve`/`Reject`) is no longer a core command: the
    /// runtime tool executor owns it (#59) and never reaches the session loop.
    ToolResult(String, Vec<ContentPart>, bool, Option<u64>, Option<i32>),
    /// Switch the live permission mode by name (ADR-0207) — carried opaquely,
    /// like the field it sets ([`Session::mode`]). Core holds no table to
    /// validate the name against, so this always succeeds: see the handler
    /// for the always-succeed / stash-deferred shape it shares with
    /// [`SetGeneration`][SessionCmd::SetGeneration].
    SetMode(String),
    /// Switch the live model/provider (`provider`, `model`) — #218. Re-resolves
    /// against [`EngineConfig::model_resolver`][crate::EngineConfig] and rebuilds
    /// `Session::llm` without restarting the engine.
    SetModel(String, String),
    /// Live-adjust generation knobs (#374, ADR-0094): partial overrides merged
    /// onto `Session::generation` via `GenerationParams::apply_overrides`.
    SetGeneration(entanglement_provider::GenerationParams),
    /// Set display metadata (`name`, `action`): `None` leaves a field
    /// untouched, `Some("")` clears it. Applied immediately — never stashed —
    /// like [`ChildSpawned`][SessionCmd::ChildSpawned], since `action` exists
    /// precisely to change mid-turn. Always acks with
    /// [`OutEvent::SessionMetaChanged`] carrying the full merged values. The
    /// third field is `if_unset` (#553): when `true`, `name` only applies if
    /// the session has no name yet.
    SetSessionMeta(Option<String>, Option<String>, bool),
    /// Replace the session's live tool overlay (#539, ADR-0149): the full
    /// [`ToolOverlayEntry`] list whose matching tools exist for this session
    /// regardless of the profile mask. Always succeeds and emits
    /// [`OutEvent::ToolOverlayChanged`] with the effective list.
    SetToolOverlay(Vec<ToolOverlayEntry>),
    /// Single out-of-band LLM op (`op`, `args`, #324) — `"compact"` today.
    Oneshot(String, serde_json::Value),
    /// Revert tracked file edits after a turn (#731, ADR-0212) through
    /// `EngineConfig::file_reverter`. Deferred mid-turn/paused like
    /// `SetModel`: a revert must not race the turn's own tool writes.
    RevertFiles(Option<u32>),
    /// Fork this session into a new root (#786, ADR-0223), whole or cut after
    /// a turn. Deferred mid-turn/paused like `RevertFiles`, so the fork is
    /// always taken at rest and never carries a parked batch.
    Fork(Option<u32>),
    /// Set the working directory (#764, ADR-0224) — already validated and
    /// absolute. Deferred mid-turn/paused like `SetModel`, so every call of
    /// one turn runs under one workdir.
    SetWorkdir(String),
    Stop,
    /// Hold the session at `AgentState::Paused` (#516, ADR-0208) — never
    /// interrupts an in-flight round (a mid-stream arrival is stashed by the
    /// existing generic mechanism in `stream.rs` and applied once the turn
    /// ends — unlike `SetMode`, which applies even mid-stream). Idempotent.
    Pause,
    /// Lift a hold placed by `Pause` (#516, ADR-0208). A no-op if not paused.
    Unpause,
    /// Evict this session from memory without tombstoning its id (#318,
    /// ADR-0077). The task emits [`OutEvent::SessionHibernated`], drops its shared
    /// seq counter, and exits — dropping `Session` (the `Context`/history). The
    /// supervisor has already removed the map entry, so no `Prompt` reaches a dead
    /// task; the id stays resumable via [`Holly::resume`][crate::Holly::resume].
    /// Routed by the supervisor on [`InMsg::HibernateSession`]; the sender is
    /// dropped alongside so a turn parked mid-stream unwinds to this teardown
    /// (stop-then-hibernate) rather than stranding.
    Hibernate,
    /// A sub-agent this session spawned came to life — append it to
    /// [`Session::children`][crate::session::Session::children]. Sent by the
    /// supervisor on [`InMsg::Spawn`] to the *parent* task, mirroring the
    /// `parent_links` edge it records. A pure state update: applied immediately,
    /// never stashed, so a mid-turn spawn reflects at once.
    ChildSpawned(SessionId),
    /// A child of this session was retired (its sub-tree closed) — remove it from
    /// [`Session::children`][crate::session::Session::children]. Sent by the
    /// supervisor on the [`InMsg::CloseSession`] cascade to the (still-live)
    /// parent of the closed sub-tree root.
    ChildClosed(SessionId),
}
