//! The live reasoning turn: assemble the advertised tool set, stream the LLM
//! response, and either finish the turn or *park* it on a batch of tool calls
//! (#270, ADR-0061). Parking is explicit state ([`TurnState`]) — the whole
//! batch is emitted as `ToolExec` up front and control returns to the session
//! loop, which resolves `ToolResult`s (any order) and re-enters [`drive_turn`]
//! when the batch drains. Separable from the replay fold (pure state
//! reconstruction) in `session/replay.rs`.

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_tool_call, emit_tool_exec, emit_turn_error, emit_usage, next_seq};
use super::stream::{stream_round, StreamedRound};
use super::summarize::{summarize, SummarizeOutcome};
use super::turn_state::TurnState;
use super::{Session, SessionCmd};
use crate::protocol::{AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::{StopReason, ToolSpec};

/// How many trailing messages auto-summarize asks to keep verbatim (#398,
/// ADR-0103), so the turn's own most recent exchange isn't paraphrased away.
/// `Context::safe_kept` clamps this to the nearest safe turn boundary, so the
/// exact number is a soft target, not a guarantee — a request deep in an
/// unfinished tool round-trip can collapse to `0`.
const AUTO_COMPACT_KEEP_TAIL: usize = 4;

/// Injected as a user-role message when a round ends with no tool calls and
/// an ambiguous stop_reason (ADR-0118) — steers a possibly-truncated model to
/// either finish the action or confirm completion, instead of silently ending
/// the turn.
const AMBIGUOUS_STOP_NUDGE: &str =
    "[system] Your previous response may have been cut off before completing. \
     If you intended to take an action, call the appropriate tool now. If you \
     are finished, reply with a short confirmation.";

/// A confident, deliberate stop — the only kind that ends a turn with no tool
/// calls. Everything else (`None`, `Other`, or a contradictory `ToolUse` with
/// zero actual tool calls) is ambiguous and gets a bounded, nudged retry
/// instead (ADR-0118) — see the classification at the bottom of `run_round`.
fn is_confident_stop(stop_reason: Option<StopReason>) -> bool {
    matches!(
        stop_reason,
        Some(StopReason::EndTurn) | Some(StopReason::MaxTokens) | Some(StopReason::StopSequence)
    )
}

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

    // Ambiguous-stop retry loop (ADR-0118): normally one LLM round-trip either
    // parks on tool calls or ends the turn on the first pass. A round that
    // ends with no tool calls *and* an ambiguous stop_reason (the stream
    // closed without a confident signal — e.g. Ollama dropping the connection
    // mid-generation) loops back for another round-trip in place, instead of
    // silently committing the truncated reply as a finished turn.
    loop {
        // Bound the inner LLM→tool loop (#177). Each round is one LLM
        // round-trip that may fan out into tool calls; a model wedged in a
        // tool loop (or persistently ambiguous, see below) would otherwise
        // run forever. The counter lives on `TurnState`, reset per prompt — a
        // legitimate long session (many prompts) is never capped, only a
        // single runaway turn. User-configurable via `max_turns` (default
        // 200); an ambiguous-stop retry still consumes this budget too, so it
        // remains the hard outer backstop regardless of
        // `max_ambiguous_stop_retries`.
        let turn = s.turn.get_or_insert_with(TurnState::default);
        turn.iterations += 1;
        if turn.iterations > max_turns {
            let _ = events.send(OutEvent::Error {
                session: session.clone(),
                seq: next_seq(&s.seq),
                message: format!(
                    "exceeded maximum turn limit ({max_turns}) - possible infinite loop"
                ),
            });
            return RoundOutcome::TurnEnded;
        }

        // Fold any user prompts that arrived mid-turn into the live context
        // before the next model request (#182). This is steering: guidance
        // sent while the turn is running reaches the model on the very next
        // round-trip — the same way a queued user message folds into the next
        // request — instead of replaying as a separate turn after `Done`.
        // Only reachable when the previous round emitted tool calls or an
        // ambiguous stop (a confidently-ended reply returns below instead), so
        // a prompt sent after the model's final answer still correctly starts
        // a fresh turn via the stash. Non-`Prompt` commands (`SetAgent`) stay
        // stashed for the session loop to handle once this turn ends.
        let mut i = 0;
        while i < stash.len() {
            if matches!(stash[i], SessionCmd::Prompt(_)) {
                if let Some(SessionCmd::Prompt(content)) = stash.remove(i) {
                    tracing::debug!("folding mid-turn prompt into live context");
                    s.ctx.push_user_content(content);
                }
            } else {
                i += 1;
            }
        }

        // Keep the request inside the model's real context window (#178).
        // Over budget, first try an LLM-generated summary in place (#398,
        // ADR-0103) — far less lossy than placeholder pruning, and the
        // natural default since a turn mid-flight has no head to fork a
        // copy-on-write `/compact` into. If that's disabled, skipped by its
        // own guard, or still doesn't fit, fall back to the prune-only
        // `Context::compact`; if even that doesn't fit, refuse the turn —
        // sending an over-window request just burns a paid round-trip and
        // errors at the provider. Refusing ends the turn cleanly (Error +
        // Done + Status) so a one-shot head unblocks.
        if !s.ctx.within_limit() {
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
            } else {
                emit_turn_error(
                    session,
                    &s.seq,
                    events,
                    format!(
                        "context window exceeded: {after} tokens estimated after \
                         compaction, over the {}-token budget — start a new session \
                         or shorten the request",
                        s.ctx.limit()
                    ),
                );
                return RoundOutcome::TurnEnded;
            }
        }

        // System prompt: the active profile's own, unless a per-turn
        // `system_prompt_resolver` is wired (#310, ADR-0078). An embedder
        // whose prompt is user-editable content (a site serving it from a CMS
        // page) consults it here so an edit lands on this turn with no engine
        // respawn; a `None` return falls back to the profile's static prompt.
        // Resolved as an owned `String` up front so `stream_round` borrows
        // nothing extra off `s`.
        let system_prompt: String = cfg
            .system_prompt_resolver
            .as_ref()
            .and_then(|resolve| resolve(session, &s.profile))
            .unwrap_or_else(|| s.profile.system_prompt.clone());

        let (text_buf, tool_calls, finish) =
            match stream_round(session, rx, s, events, stash, &specs, &system_prompt).await {
                StreamedRound::Complete {
                    text,
                    tool_calls,
                    finish,
                } => (text, tool_calls, finish),
                // Stop / inbox close mid-stream — the turn ends but the
                // session stays alive (cancel semantics, ADR-0017).
                StreamedRound::Cancelled => return RoundOutcome::Cancelled,
                // Partial committed + Error/Done already emitted (#181).
                StreamedRound::Failed => return RoundOutcome::TurnEnded,
            };

        // Fold this round-trip's usage into the session total and emit the
        // delta (#192). A `max_tokens`-truncated reply is surfaced as a
        // recoverable warning so it no longer commits silently as a clean
        // turn.
        let stop_reason: Option<StopReason> = finish.as_ref().and_then(|(sr, _)| *sr);
        if let Some((sr, usage)) = finish {
            // A live model switch (#218) prices the turn under the switched
            // model.
            let model = s
                .model
                .as_deref()
                .or(s.profile.model.as_deref())
                .or(cfg.default_model.as_deref());
            let cost = model
                .and_then(|m| cfg.pricing.get(m))
                .map(|p| p.cost_usd(&usage));
            emit_usage(session, s, events, &usage, cost);
            if sr == Some(StopReason::MaxTokens) {
                let _ = events.send(OutEvent::Error {
                    session: session.clone(),
                    seq: next_seq(&s.seq),
                    message: "model response truncated: hit the max output token limit \
                              (stop reason: max_tokens)"
                        .to_string(),
                });
            }
        }

        s.ctx.push_assistant(text_buf.clone(), tool_calls.clone());
        tracing::debug!(
            text_len = text_buf.len(),
            tool_calls_count = tool_calls.len(),
            context_messages = s.ctx.messages().len(),
            "assistant message pushed"
        );

        if !tool_calls.is_empty() {
            // Real tool calls: recovered from any prior ambiguity.
            if let Some(turn) = s.turn.as_mut() {
                turn.ambiguous_retries = 0;
            }
            // Emit the whole batch up front and park (#270). Every tool is a
            // protocol round-trip (#58): the runtime tool executor (or any
            // external resolver) answers each `ToolExec` with
            // `InMsg::ToolResult`; core makes no policy call (#59). Calls
            // execute concurrently — results resolve in any order against the
            // pending set (ADR-0061; deliberate change from the serial
            // in-call-order dispatch this replaced).
            for call in &tool_calls {
                emit_tool_call(events, session, &call.id, &call.name, &call.input, &s.seq);
                emit_tool_exec(events, session, call, &s.profile.name, &s.seq);
            }
            if let Some(turn) = s.turn.as_mut() {
                turn.begin_batch(tool_calls);
            }
            return RoundOutcome::Parked;
        }

        if is_confident_stop(stop_reason) {
            tracing::debug!("no tool calls, confident stop - emitting Done");
            let _ = events.send(OutEvent::Done {
                session: session.clone(),
                seq: next_seq(&s.seq),
            });
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Done,
            });
            return RoundOutcome::TurnEnded;
        }

        // Ambiguous stop (ADR-0118): `None`, `Other`, or a contradictory
        // `ToolUse` with zero actual tool calls — the stream ended without a
        // clean signal that the model was actually finished. Retry in place,
        // bounded by `max_ambiguous_stop_retries`, so a persistently confused
        // model still can't loop forever.
        let turn = s
            .turn
            .as_mut()
            .expect("turn set above by get_or_insert_with");
        if turn.ambiguous_retries >= cfg.max_ambiguous_stop_retries {
            tracing::debug!(
                ?stop_reason,
                retries = turn.ambiguous_retries,
                "ambiguous stop - retry budget exhausted, emitting Done"
            );
            // A cap of 0 is a deliberate opt-out (ADR-0118): it restores the
            // pre-ADR-0118 behavior of silently committing the reply, so the
            // very first ambiguous stop must *not* surface a warning it never
            // asked for. Only emit the warning when at least one retry was
            // budgeted (and thus actually attempted).
            if cfg.max_ambiguous_stop_retries > 0 {
                let _ = events.send(OutEvent::Error {
                    session: session.clone(),
                    seq: next_seq(&s.seq),
                    message: format!(
                        "model stop was ambiguous (stop reason: {stop_reason:?}) after \
                         {} retries - response may be incomplete",
                        cfg.max_ambiguous_stop_retries
                    ),
                });
            }
            let _ = events.send(OutEvent::Done {
                session: session.clone(),
                seq: next_seq(&s.seq),
            });
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Done,
            });
            return RoundOutcome::TurnEnded;
        }
        turn.ambiguous_retries += 1;
        tracing::debug!(
            ?stop_reason,
            retries = turn.ambiguous_retries,
            "ambiguous stop - nudging and retrying"
        );
        s.ctx.push_user(AMBIGUOUS_STOP_NUDGE);
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
