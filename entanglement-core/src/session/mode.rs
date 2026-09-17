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

/// Text appended as the last message of every request, reflecting `mode` as
/// of the round being built right now. Deliberately terse: it only names
/// `mode`, trusting the cached system-prompt preamble
/// ([`EngineConfig::modes_preamble`][crate::EngineConfig]) to explain what the
/// name means.
pub(super) fn mode_notice(mode: &str) -> String {
    format!("[mode: {mode}]")
}

#[cfg(test)]
mod tests {
    use super::mode_notice;

    #[test]
    fn notice_names_the_mode() {
        assert_eq!(mode_notice("research"), "[mode: research]");
    }
}
