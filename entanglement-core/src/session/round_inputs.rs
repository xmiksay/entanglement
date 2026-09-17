//! The two per-round inputs a turn resolves once and hands to every attempt:
//! the advertised tool specs and the system prompt. Split out of
//! `session/turn.rs` (ADR-0202) because compaction needs the very same pair:
//! the session-backend summarization request replays the session's own
//! system + tools ahead of the history so the provider's cached prefix is
//! hit — a request built from anything else would miss the cache from the
//! tools block onward. Both `turn.rs` and `ops.rs`'s manual `/compact` call
//! these, so there is exactly one place that decides what a session sends.

use super::Session;
use crate::protocol::SessionId;
use crate::{EngineConfig, SessionModel};
use entanglement_provider::ToolSpec;

/// Tool set advertised to the model = **every** spec the config provides for
/// this session. Advertisement is decoupled from enforcement: the profile
/// mask (#116, ADR-0038), the session tool overlay (#539, ADR-0149) and an
/// active skill's `allowed_tools` (#400, ADR-0106) do not filter here — they
/// are enforced exclusively by the runtime's dispatch gate, exactly as the
/// `Allow`/`Ask`/`Deny` permission ladder always has been. WHY: every
/// mid-session change to this array (an overlay toggle, a skill mask)
/// invalidated the provider's prompt cache from the tools block onward — i.e.
/// the whole prompt. A
/// surface that is stable within a session keeps that cache warm; the only
/// thing traded away is "the model cannot even attempt the call", and every
/// attempt is now visibly declined at dispatch instead.
///
/// The base tool schemas are engine-global (`tool_specs`) unless a
/// per-session `tool_spec_resolver` is wired (#308, ADR-0076): a multi-tenant
/// embedder consults it here to vary the advertised surface per session (each
/// user's discovered MCP-server tools, a site's restriction) on one `Holly`.
/// Its output *replaces* the static list for this session. Consulted fresh
/// every turn, so a backing-store edit lands on the next turn with no engine
/// respawn — and, because nothing downstream filters it, the resolver is the
/// single seam that shapes a session's base surface. It is handed the
/// session's bound model so it can pin model-derived facts at first
/// resolution; callers resolve specs before [`resolve_system_prompt`] in
/// every round, which a prompt resolver reading those pins relies on.
///
/// The old per-profile spawn-roster split (#119, ADR-0040) is retired
/// (ADR-0207 §6/§9): spawning is unconditional now, so the `agent_*` family
/// is a constant roster the runtime folds into the plain shared
/// `cfg.tool_specs` like every other runtime-owned tool — there is no more
/// per-profile table to append here. That is the whole point: the advertised
/// array no longer varies by profile — moot anyway, now that an agent is
/// fixed for a session's whole life (ADR-0207 §9, `SetAgent` is gone).
pub(super) fn resolve_specs(cfg: &EngineConfig, session: &SessionId, s: &Session) -> Vec<ToolSpec> {
    match &cfg.tool_spec_resolver {
        Some(resolve) => resolve(
            session,
            SessionModel {
                provider: s.provider.as_deref(),
                model: s.model.as_deref(),
            },
        ),
        None => cfg.tool_specs.clone(),
    }
}

/// System prompt: the active profile's own, unless a per-turn
/// `system_prompt_resolver` is wired (#310, ADR-0078). An embedder whose
/// prompt is user-editable content (a site serving it from a CMS page)
/// consults it here so an edit lands on this turn with no engine respawn; a
/// `None` return falls back to the profile's static prompt. Returned owned so
/// the caller borrows nothing extra off `s` while streaming. A resolver may
/// be a remote fetch, so a round calls this exactly once — compaction reuses
/// the round's value rather than resolving again.
///
/// [`EngineConfig::modes_preamble`] (ADR-0207 §9), when set, is appended
/// after the resolved base prompt. It describes what modes exist — the
/// runtime's mode table, never core's to author — and stays **static** across
/// every session/turn, so appending it here never costs the cache: it sits in
/// the same prefix a session's very first request already hits. The *current*
/// mode is a different animal entirely and never touches this function — see
/// `mode::mode_notice`.
pub(super) fn resolve_system_prompt(
    cfg: &EngineConfig,
    session: &SessionId,
    s: &Session,
) -> String {
    let base = cfg
        .system_prompt_resolver
        .as_ref()
        .and_then(|resolve| resolve(session, &s.profile))
        .unwrap_or_else(|| s.profile.system_prompt.clone());
    match cfg.modes_preamble.as_deref() {
        Some(preamble) if !preamble.is_empty() => format!("{base}\n\n{preamble}"),
        _ => base,
    }
}
