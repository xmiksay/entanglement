//! The live reasoning turn: assemble the advertised tool set (every spec the
//! config provides — masks are dispatch-only, see `run_round`), stream the LLM
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

use super::compaction_request::SessionPrefix;
use super::emit::{emit_turn_error, emit_usage};
use super::fork::{fork_successor, Compaction, Forked};
use super::round::{run_attempt, RoundAttempt, RoundSetup};
use super::round_inputs::{resolve_specs, resolve_system_prompt};
use super::summarize::{compose_report, summarize, AuxBackend, SummarizeOutcome};
use super::transcript::render_transcript;
use super::{Session, SessionCmd};
use crate::context::estimate_text_tokens;
use crate::protocol::{AgentState, CompactionMode, OutEvent, SessionId, UsagePurpose};
use crate::EngineConfig;

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
    /// The round's context-window gate compacted, which forks (ADR-0205): this
    /// session is retired and the turn continues in its successor. Nothing
    /// more may be emitted here.
    Forked,
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
) -> Forked {
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });
    match run_round(session, rx, s, events, stash, cfg).await {
        RoundOutcome::Forked => {
            s.turn = None;
            return Forked::Yes;
        }
        RoundOutcome::Parked => {
            // The batch's `ToolExec`s have all been emitted and the turn is now
            // parked on `ToolResult`s — distinguish this from model generation
            // (`Thinking`) so a head can show "running a command" vs "LLM is
            // generating" (ADR-0139). Covers both `Allow` tools (executing) and
            // `Ask` tools (about to flip to `WaitingApproval`): the runtime's
            // `WaitingApproval` emit lands just after this, so the user sees a
            // brief flash of `Working` that correctly precedes the approval
            // prompt.
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Working,
            });
        }
        RoundOutcome::TurnEnded | RoundOutcome::Cancelled => s.turn = None,
    }
    Forked::No
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
    // Both per-round inputs resolve exactly once, before the context-window
    // gate and the ADR-0118 retry loop below (#436): an ambiguous-stop retry
    // re-streams in place and must not repeat a potentially remote
    // `system_prompt_resolver` fetch (ADR-0078), and compaction reuses this
    // very pair so its structured request replays the round's cached prefix
    // (ADR-0202) instead of resolving it a second time.
    let specs = resolve_specs(cfg, session, s);
    let system_prompt = resolve_system_prompt(cfg, session, s);
    let max_turns = cfg.max_turns.max(1);

    // The gate also runs once per round, not per attempt: a retry's own
    // pushed nudge + partial text could otherwise trip an unneeded LLM
    // summarization (#436).
    if !s.ctx.within_limit() {
        let prefix = SessionPrefix {
            system_prompt: &system_prompt,
            specs: &specs,
            cache_key: session.0.as_str(),
        };
        if let Some(outcome) = enforce_context_window(session, s, events, cfg, prefix).await {
            return outcome;
        }
    }

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
/// this round, by **compacting into a successor session** (ADR-0205). Three
/// recovery steps, in order:
///
/// 1. an LLM-generated summary of the oldest history (#398, ADR-0103, gated by
///    `EngineConfig::auto_compact`), forked as `CompactionMode::Summary`;
/// 2. failing that, the prune-only fallback — placeholder-prune the oldest
///    tool outputs — forked as `CompactionMode::Prune` (#450, ADR-0121's
///    silent in-place prune is retired: a head must follow the fork, so it is
///    announced like any other compaction);
/// 3. failing both, refuse the turn — sending an over-window request just
///    burns a paid round-trip and errors at the provider.
///
/// Neither compacting step touches `s.ctx`: the summary reads it and the prune
/// works on a clone, so the source session's history is exactly what its own
/// log already says (ADR-0205's replay-fidelity guarantee). Returns
/// `Some(outcome)` in every case — `Forked` when the turn moved to a
/// successor, `TurnEnded` on the refusal — and `None` only when the request
/// already fits and the round should proceed as-is.
///
/// `prefix` is the round's already-resolved system prompt + specs, handed
/// through so the summary request can replay them (ADR-0202).
async fn enforce_context_window(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    prefix: SessionPrefix<'_>,
) -> Option<RoundOutcome> {
    if cfg.auto_compact {
        if let Some(outcome) = try_summary_fork(session, s, events, cfg, prefix).await {
            return Some(outcome);
        }
    }
    if let Some(outcome) = try_prune_fork(session, s, events, cfg) {
        return Some(outcome);
    }
    emit_turn_error(
        session,
        &s.seq,
        events,
        format!(
            "context window exceeded: {} tokens estimated, over the {}-token \
             budget, and compaction could not reclaim enough — start a new \
             session or shorten the request",
            s.ctx.estimated_tokens(),
            s.ctx.limit()
        ),
    );
    Some(RoundOutcome::TurnEnded)
}

/// Step 1: summarize the oldest history with the model and fork a successor
/// seeded with that summary plus the verbatim kept tail (#397/ADR-0102's
/// `compose_report`, the same seed the manual `/compact` builds).
///
/// `None` on any failure — a tripped summarize guard (oversized transcript or
/// tail, an LLM error, a truncated summary) is expected and unremarkable here,
/// and the caller falls through to the prune fallback.
async fn try_summary_fork(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    prefix: SessionPrefix<'_>,
) -> Option<RoundOutcome> {
    // Model resolution mirrors the request field: a live switch (#218)
    // overrides the profile's pinned model; `None` falls back to the backend's
    // own default.
    let model = s.model.as_deref().or(s.agent.model.as_deref());
    // Same `summarize` aux-model pin as the manual `/compact` path (Issue 5):
    // an overflow recovery is a side transformation too, so it runs on the
    // pinned backend when one is set.
    let mut aux = AuxBackend::for_summarize(cfg);
    let backend = aux.resolve(&mut *s.llm, model, s.generation);
    let model = backend.model.map(str::to_string);

    let outcome = summarize(&s.ctx, backend, prefix, AUTO_COMPACT_KEEP_TAIL, None).await;
    let SummarizeOutcome {
        summary,
        kept,
        tail_rendered,
        finish,
    } = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::debug!(
                reason = %e,
                "auto-compact summarization unavailable, falling back to pruning"
            );
            return None;
        }
    };

    let seed = compose_report(&summary, kept, tail_rendered.as_deref());
    let fork = fork_successor(
        session,
        s,
        events,
        cfg,
        Compaction {
            seed: &seed,
            kept,
            auto: true,
            mode: CompactionMode::Summary,
        },
    )?;
    if let Some((_, usage)) = finish {
        let priced_model = model.as_deref().or(cfg.default_model.as_deref());
        let cost = priced_model
            .and_then(|m| cfg.pricing.get(m))
            .map(|p| p.cost_usd(&usage));
        emit_usage(session, s, events, &usage, cost, UsagePurpose::Compaction);
    }
    fork.dispatch();
    Some(RoundOutcome::Forked)
}

/// Step 2: placeholder-prune the oldest tool outputs and fork a successor
/// seeded with the pruned history.
///
/// The prune runs on a **clone** of the live context, so the source's history
/// is never rewritten (ADR-0205). The successor's seed is a single prompt, so
/// that pruned history ships as a rendered transcript — guarded against the
/// input budget, since a transcript that doesn't fit either would just move
/// the overflow into the successor. `None` when pruning can't bring the
/// history under budget (a single oversized message) or the fork is
/// unavailable; the caller then refuses the turn.
fn try_prune_fork(
    session: &SessionId,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
) -> Option<RoundOutcome> {
    let before = s.ctx.estimated_tokens();
    let mut pruned = s.ctx.clone();
    if !pruned.compact() {
        return None;
    }
    let seed = render_transcript(pruned.messages());
    let seed_tokens = estimate_text_tokens(&seed);
    if seed_tokens > s.ctx.limit() {
        tracing::debug!(
            seed_tokens,
            limit = s.ctx.limit(),
            "prune fork: the pruned transcript still overflows the budget"
        );
        return None;
    }
    tracing::info!(
        before,
        after = seed_tokens,
        limit = s.ctx.limit(),
        "pruned the context to fit the model's window; forking a successor"
    );
    let fork = fork_successor(
        session,
        s,
        events,
        cfg,
        Compaction {
            seed: &seed,
            // The prune keeps no verbatim tail distinct from its head: the
            // whole pruned history *is* the seed.
            kept: 0,
            auto: true,
            mode: CompactionMode::Prune,
        },
    )?;
    fork.dispatch();
    Some(RoundOutcome::Forked)
}
