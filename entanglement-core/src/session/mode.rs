//! The mode notice (ADR-0207 §9): the one-line message telling the model
//! which permission mode the session is running under right now.
//!
//! Why an appended message and not a system-prompt rewrite: the system
//! prompt sits in the provider's cached prefix (ADR-0202), so rewriting it on
//! every [`SetMode`][super::SessionCmd::SetMode] would invalidate the whole
//! prefix — tools, system, and every message before the edit. Appending a
//! message costs nothing: the existing prefix is untouched, the history only
//! grows, exactly like every other append-only turn. Core authors only this
//! terse pointer; the static "what each mode means" text is the runtime's own
//! [`EngineConfig::modes_preamble`][crate::EngineConfig], folded into the
//! *system* prompt once (see [`super::round_inputs::resolve_system_prompt`])
//! because it never varies — core does not own the mode table, so it cannot
//! author that text either.
//!
//! Why this is built **fresh every round** ([`super::stream::stream_round`])
//! instead of pushed once into [`Context`][crate::context::Context] like the
//! ADR-0118 ambiguous-stop nudge: a persisted push would desync live vs.
//! replayed history. [`Session::mode`][super::Session] changes are carried by
//! [`InMsg::SetMode`][crate::protocol::InMsg::SetMode], which — like every
//! non-`Prompt`/`Stop` command — is dropped by the log's pairing step
//! (`entanglement-runtime`'s `pair_records`, which only ever queues `Prompt`/
//! `Stop` for the *next* persisted `Out` record). A session's very first
//! `Prompt` is *always* paired with the earliest `Out` record on disk —
//! `SessionStarted` for a lazily-created root, the child's own next event for
//! a spawned one (#421) — so `Session::replay` folds that prompt into `ctx`
//! at the very first iteration, before any later record (including a
//! session-start `ModeChanged`) gets a chance to run. A one-time push at
//! session start would therefore land *after* the first prompt on replay but
//! *before* it live — a real, provable divergence (caught by this stage's own
//! test suite: several `replay_equality`/`hibernate`/`invoke_envelope` cases
//! failed on exactly this ordering before the fix). Deriving the notice from
//! `mode` at request time sidesteps the hazard entirely: nothing about it is
//! persisted or pairing-dependent, so replay never needs to reconstruct it —
//! reconstructing `Session::mode` itself (a plain overwrite fold, see
//! `replay.rs`) is enough.
//!
//! One call site pushes this notice: `stream.rs`, appended as the last
//! message of every request — after the real conversation, right before the
//! model replies — so it always reflects `Session::mode` as of *this* round,
//! covering session start and every switch uniformly with no special-casing.
//!
//! A plain per-round notice has its own gap, though: a switch is silent
//! otherwise — `[mode: plan]` just becomes `[mode: build]` next round, with
//! nothing marking the edge, so a model mid-task can keep acting on its
//! earlier understanding. [`mode_notice_with_transition`] closes it: the
//! *first* request built after a real change names what the mode changed
//! from too, consuming [`Session::mode_transition_from`][super::Session] (set
//! by the same `SetMode` handling that updates `mode` itself) so the callout
//! fires exactly once and every later round falls back to the plain form.
//! Same ephemerality as the base notice and for the same reason: it lives
//! only on `Session` state and the outgoing request, never `Context`, never
//! replayed — a resumed session has no live "next round" to attach a
//! transition to, so it simply shows the plain notice.

use tokio::sync::broadcast;

use crate::protocol::{OutEvent, SessionId};

/// Text appended as the last message of every request, reflecting `mode` as
/// of the round being built right now. Deliberately terse: it only names
/// `mode`, trusting the cached system-prompt preamble
/// ([`EngineConfig::modes_preamble`][crate::EngineConfig]) to explain what the
/// name means.
pub(super) fn mode_notice(mode: &str) -> String {
    format!("[mode: {mode}]")
}

/// [`mode_notice`], plus a one-shot **transition** callout (#560 follow-up):
/// on the first request after a `SetMode` actually changed the mode, name
/// what it changed *from* too — `[mode: build — changed from plan]` — since
/// the plain per-round notice alone silently swaps `[mode: plan]` for
/// `[mode: build]` with nothing marking the edge, and a model mid-task can
/// easily keep acting on its earlier understanding of what it may do. Every
/// request after that reverts to the plain form.
///
/// `from` is [`Session::mode_transition_from`][super::Session] **taken**
/// (not borrowed) by the caller, once per round — see that field's own doc
/// for why the marker is consumed exactly once and never persisted/replayed.
/// Core never attributes *who* changed the mode (a user `/mode`, a
/// `propose_plan`/`request_mode` approval): it only ever sees
/// `InMsg::SetMode { mode }`, and "changed from X" is accurate regardless of
/// the cause. `from == mode` (a switch that net cancelled itself out before
/// the next round, e.g. `plan → build → plan`) is treated as no transition —
/// there is nothing true to call out.
pub(super) fn mode_notice_with_transition(mode: &str, from: Option<String>) -> String {
    match from {
        Some(from) if from != mode => format!("[mode: {mode} — changed from {from}]"),
        _ => mode_notice(mode),
    }
}

/// Apply a `SetMode` **now** and ack it with `ModeChanged` — from the idle
/// session loop and from mid-stream alike (`stream.rs`). Never deferred: the
/// runtime grades every tool call against the mode it last saw announced, so
/// a switch held until the turn ends would let calls already streaming under
/// the old mode dispatch with its authority (e.g. a `/mode research` typed
/// while `build` is mid-reply). Takes the two fields rather than `&mut
/// Session` because the mid-stream caller holds `s.llm` borrowed.
///
/// `transition_from` is only written when this genuinely changes the mode
/// and nothing is pending yet: several switches landing before the model's
/// next round (a plan-approval cascade, a fast re-typed `/mode`) collapse
/// into one notice naming the *original* mode, not an intermediate one.
pub(super) fn apply_set_mode(
    current: &mut String,
    transition_from: &mut Option<String>,
    mode: String,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
) {
    if mode != *current {
        transition_from.get_or_insert_with(|| current.clone());
    }
    *current = mode.clone();
    let _ = events.send(OutEvent::ModeChanged {
        session: session.clone(),
        mode,
    });
}

#[cfg(test)]
mod tests {
    use super::{mode_notice, mode_notice_with_transition};

    #[test]
    fn notice_names_the_mode() {
        assert_eq!(mode_notice("research"), "[mode: research]");
    }

    #[test]
    fn transition_names_both_modes_once() {
        assert_eq!(
            mode_notice_with_transition("build", Some("plan".to_string())),
            "[mode: build — changed from plan]"
        );
    }

    #[test]
    fn no_transition_falls_back_to_the_plain_notice() {
        assert_eq!(mode_notice_with_transition("build", None), "[mode: build]");
    }

    #[test]
    fn a_transition_that_nets_to_the_same_mode_is_not_a_transition() {
        assert_eq!(
            mode_notice_with_transition("plan", Some("plan".to_string())),
            "[mode: plan]"
        );
    }
}
