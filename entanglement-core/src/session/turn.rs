//! The live reasoning turn: assemble the advertised tool set, stream the LLM
//! response, and either finish the turn or *park* it on a batch of tool calls
//! (#270, ADR-0061). Parking is explicit state ([`TurnState`]) — the whole
//! batch is emitted as `ToolExec` up front and control returns to the session
//! loop, which resolves `ToolResult`s (any order) and re-enters [`drive_turn`]
//! when the batch drains. Separable from the replay fold (pure state
//! reconstruction) in `session/replay.rs`. The per-attempt streaming and the
//! ADR-0118 ambiguous-stop retry live in `session/round.rs` (#436) — this
//! module owns the setup that only needs to run once per round (tool specs,
//! the context-window gate, system prompt resolution) and the small driver
//! loop that retries in place without repeating it.

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_turn_error, emit_usage, next_seq};
use super::round::{run_attempt, RoundAttempt, RoundSetup};
use super::summarize::{summarize, SummarizeOutcome};
use super::{Session, SessionCmd};
use crate::protocol::{AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::ToolSpec;

/// How many trailing messages auto-summarize asks to keep verbatim (#398,
/// ADR-0103), so the turn's own most recent exchange isn't paraphrased away.
/// `Context::safe_kept` clamps this to the nearest safe turn boundary, so the
/// exact number is a soft target, not a guarantee — a request deep in an
/// unfinished tool round-trip can collapse to `0`.
const AUTO_COMPACT_KEEP_TAIL: usize = 4;

/// How one LLM round-trip left the turn.
pub(crate) enum RoundOutcome {
    /// The model answered without tool calls (or the round failed / hit the
    /// turn limit): the turn is over.
    TurnEnded,
    /// The round ended in tool calls: the batch was emitted, `Session::turn`
    /// holds the pending set, and the session loop resolves it.
    Parked,
    /// `Stop` / inbox close preempted the round (ADR-0017).
    Cancelled,
}

/// Advance the live turn until it parks on tool results or ends. The caller
/// (the session loop) owns `Session::turn`: `Some` on entry; left `Some` only
/// when parked, cleared on any other outcome. Cancel semantics (ADR-0017):
/// context is preserved in every case and the session task stays alive.
pub(crate) async fn drive_turn(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    cfg: &EngineConfig,
) {
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });
    match run_round(session, rx, s, events, stash, cfg).await {
        RoundOutcome::Parked => {} // s.turn holds the pending batch
        RoundOutcome::TurnEnded | RoundOutcome::Cancelled => s.turn = None,
    }
}

/// One LLM round-trip: fold stashed prompts (ADR-0058), enforce the turn
/// budget (#177) and context window (#178), stream the reply, and commit it.
/// A reply with tool calls emits the whole batch — the per-call
/// (`ToolCall`, `ToolExec`) pair for every call up front — records it as
/// [`TurnState::pending`], and parks.
async fn run_round(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    cfg: &EngineConfig,
) -> RoundOutcome {
    // Tool set advertised to the model = host tools (from config, #61) filtered
    // by the active profile's allowlist/denylist mask (#116, ADR-0038). Core
    // caches no fixed tool set on the session; the schemas come from
    // `EngineConfig.tool_specs` at turn time. The mask is a *physical*
    // restriction — a masked tool's schema never reaches the model — layered
    // under the runtime's `Allow`/`Ask`/`Deny` dispatch, which grades only the
    // tools that survive here. `update_plan`/`update_tasks` are runtime state
    // tools now (#231, ADR-0049): they ride `tool_specs`/`profile_tool_specs`
    // and this mask like any other host tool, with zero plan-authority special
    // casing in core.
    // The base tool schemas are engine-global (`tool_specs`) unless a
    // per-session `tool_spec_resolver` is wired (#308, ADR-0076): a multi-tenant
    // embedder consults it here to vary the advertised surface per session (each
    // user's discovered MCP-server tools, a site's restriction) on one `Holly`.
    // Its output *replaces* the static list for this session — but the profile
    // mask below still filters it, so the resolver widens discovery, never
    // bypasses masking. Consulted fresh every turn, so a backing-store edit lands
    // on the next turn with no engine respawn.
    let base_specs = match &cfg.tool_spec_resolver {
        Some(resolve) => resolve(session),
        None => cfg.tool_specs.clone(),
    };
    let mut specs: Vec<ToolSpec> = base_specs
        .into_iter()
        .filter(|spec| s.profile.advertises_tool(&spec.name))
        .collect();
    // Per-profile specs (#119, ADR-0040): the active profile's spawnable roster
    // (the `agent_*` family with a target enum scoped to who *this* profile may
    // spawn) plus the plan-authorship tools (#231) live outside the shared
    // `tool_specs` so a masked schema never reaches the model. The runtime leaves
    // the entry empty for a profile that may not spawn / does not author plans.
    // Still filtered through the #116 mask, so a `disallowed_tools` list can
    // subtract even a per-profile tool.
    if let Some(profile_specs) = cfg.profile_tool_specs.get(&s.profile.name) {
        specs.extend(
            profile_specs
                .iter()
                .filter(|spec| s.profile.advertises_tool(&spec.name))
                .cloned(),
        );
    }

    let max_turns = cfg.max_turns.max(1);

    // Resolved once per round, *before* the ADR-0118 retry loop below —
    // not on every attempt. An ambiguous-stop retry mutates context with a
    // small nudge and re-streams in place; redoing this setup per retry would
    // repeat a potentially remote `system_prompt_resolver` fetch (ADR-0078)
    // and could trip the auto-compact gate into an unneeded LLM
    // summarization purely because of the retry's own pushed nudge + partial
    // text (#436).
    if !s.ctx.within_limit() {
        if let Some(outcome) = enforce_context_window(session, s, events, cfg).await {
            return outcome;
        }
    }

    // System prompt: the active profile's own, unless a per-turn
    // `system_prompt_resolver` is wired (#310, ADR-0078). An embedder whose
    // prompt is user-editable content (a site serving it from a CMS page)
    // consults it here so an edit lands on this turn with no engine respawn;
    // a `None` return falls back to the profile's static prompt. Resolved as
    // an owned `String` up front so `stream_round` borrows nothing extra off
    // `s`.
    let system_prompt: String = cfg
        .system_prompt_resolver
        .as_ref()
        .and_then(|resolve| resolve(session, &s.profile))
        .unwrap_or_else(|| s.profile.system_prompt.clone());

    // Ambiguous-stop retry loop (ADR-0118): normally one LLM round-trip
    // either parks on tool calls or ends the turn on the first pass. A round
    // that ends with no tool calls *and* an ambiguous stop_reason (the
    // stream closed without a confident signal — e.g. Ollama dropping the
    // connection mid-generation) loops back for another attempt in place
    // (`session::round::run_attempt`), instead of silently committing the
    // truncated reply as a finished turn.
    let setup = RoundSetup {
        specs: &specs,
        system_prompt: &system_prompt,
        cfg,
        max_turns,
    };
    loop {
        match run_attempt(session, rx, s, events, stash, &setup).await {
            RoundAttempt::Parked => return RoundOutcome::Parked,
            RoundAttempt::TurnEnded => return RoundOutcome::TurnEnded,
            RoundAttempt::Cancelled => return RoundOutcome::Cancelled,
            RoundAttempt::AmbiguousRetry => continue,
        }
    }
}

/// Keep the request inside the model's real context window (#178) before
/// this round. Over budget, first try an LLM-generated summary in place
/// (#398, ADR-0103) — far less lossy than placeholder pruning, and the
/// natural default since a turn mid-flight has no head to fork a
/// copy-on-write `/compact` into. If that's disabled, skipped by its own
/// guard, or still doesn't fit, fall back to the prune-only
/// `Context::compact`; if even that doesn't fit, refuse the turn — sending an
/// over-window request just burns a paid round-trip and errors at the
/// provider. Returns `Some(outcome)` only on that refusal (Error + Done +
/// Status already emitted); `None` means the request now fits and the round
/// should proceed.
async fn enforce_context_window(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
) -> Option<RoundOutcome> {
    let before = s.ctx.estimated_tokens();
    if cfg.auto_compact {
        try_auto_compact(session, s, events, cfg).await;
    }
    let fits = if s.ctx.within_limit() {
        true
    } else {
        s.ctx.compact()
    };
    let after = s.ctx.estimated_tokens();
    if fits {
        tracing::info!(
            before,
            after,
            limit = s.ctx.limit(),
            "compacted context to fit the model's window"
        );
        None
    } else {
        emit_turn_error(
            session,
            &s.seq,
            events,
            format!(
                "context window exceeded: {after} tokens estimated after compaction, \
                 over the {}-token budget — start a new session or shorten the request",
                s.ctx.limit()
            ),
        );
        Some(RoundOutcome::TurnEnded)
    }
}

/// Try an LLM-generated summary of the oldest history, mutating `s.ctx` **in
/// place** on success (#398, ADR-0103) — the fundamental split from the
/// manual `/compact`'s copy-on-write (ADR-0101): a turn mid-flight has no head
/// available to fork a new session into, so the only sound recovery is
/// compacting the live context and continuing the same turn. Silent on
/// failure: the summarize guard tripping (an oversized transcript/tail, an
/// LLM error, a truncated summary) is expected and unremarkable here — the
/// caller falls back to the prune-only `Context::compact`.
async fn try_auto_compact(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
) {
    // Model resolution mirrors the request field below: a live switch (#218)
    // overrides the profile's pinned model; `None` falls back to the backend's
    // own default.
    let model = s.model.as_deref().or(s.profile.model.as_deref());

    match summarize(
        &s.ctx,
        &mut *s.llm,
        model,
        s.generation,
        AUTO_COMPACT_KEEP_TAIL,
        None,
    )
    .await
    {
        Ok(SummarizeOutcome {
            summary,
            kept,
            finish,
            // `apply_compaction` re-derives the tail structurally from
            // `kept` against the live `ctx` — the rendered tail text is only
            // needed by the copy-on-write manual path's flat report.
            tail_rendered: _,
        }) => {
            s.ctx.apply_compaction(&summary, kept);
            let _ = events.send(OutEvent::Compacted {
                session: session.clone(),
                seq: next_seq(&s.seq),
                summary,
                kept: kept as u64,
                auto: true,
            });
            if let Some((_, usage)) = finish {
                let priced_model = model.or(cfg.default_model.as_deref());
                let cost = priced_model
                    .and_then(|m| cfg.pricing.get(m))
                    .map(|p| p.cost_usd(&usage));
                emit_usage(session, s, events, &usage, cost);
            }
        }
        Err(e) => {
            tracing::debug!(
                reason = %e,
                "auto-compact summarization unavailable, falling back to pruning"
            );
        }
    }
}
