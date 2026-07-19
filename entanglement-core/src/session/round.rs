//! One streamed attempt within a turn round, and the ADR-0118 ambiguous-stop
//! retry that can keep it going in place: bump the iteration budget, fold a
//! prompt that arrived mid-retry (#182), stream the reply, commit it, and
//! either park on tool calls, end the turn, or nudge the model and hand
//! [`RoundAttempt::AmbiguousRetry`] back to [`super::turn::run_round`]'s
//! driver loop. Split out of `turn.rs` along this retry seam (#436) so the
//! per-round setup that loop owns (system prompt resolution, the
//! context-window gate) runs once per round instead of once per retry.

use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_tool_call, emit_tool_exec, emit_turn_done, emit_usage, next_seq};
use super::stream::{stream_round, StreamedRound};
use super::turn_state::TurnState;
use super::{Session, SessionCmd};
use crate::protocol::{OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::{StopReason, ToolSpec};

/// Injected as a user-role message when a round ends with no tool calls and
/// an ambiguous stop_reason (ADR-0118) — steers a possibly-truncated model to
/// either finish the action or confirm completion, instead of silently ending
/// the turn.
const AMBIGUOUS_STOP_NUDGE: &str =
    "[system] Your previous response may have been cut off before completing. \
     If you intended to take an action, call the appropriate tool now. If you \
     are finished, reply with a short confirmation.";

/// The per-round values [`super::turn::run_round`] resolves once, before the
/// retry loop, and hands to every attempt unchanged (#436) — bundled instead
/// of passed as separate arguments so a retry can't accidentally re-derive
/// one of them.
#[derive(Clone, Copy)]
pub(super) struct RoundSetup<'a> {
    pub specs: &'a [ToolSpec],
    pub system_prompt: &'a str,
    pub cfg: &'a EngineConfig,
    pub max_turns: usize,
}

/// How one streamed attempt left the turn.
pub(super) enum RoundAttempt {
    /// The model answered with a confident stop, or the round failed / hit
    /// the turn limit / exhausted its ambiguous-retry budget: the turn is
    /// over.
    TurnEnded,
    /// The round ended in tool calls: the batch was emitted, `Session::turn`
    /// holds the pending set, and the session loop resolves it.
    Parked,
    /// `Stop` / inbox close preempted the round (ADR-0017).
    Cancelled,
    /// An ambiguous stop_reason (ADR-0118): the nudge is already pushed into
    /// context and the retry event emitted — the caller re-attempts in place
    /// without re-running its own per-round setup.
    AmbiguousRetry,
}

/// One streamed round-trip: bump the turn's iteration counter (an ambiguous
/// retry consumes this budget too, ADR-0118), fold any prompt that arrived
/// mid-retry (#182), stream the reply, commit it, and classify the stop. A
/// reply with tool calls emits the whole batch — the per-call (`ToolCall`,
/// `ToolExec`) pair for every call up front — records it as
/// [`TurnState::pending`], and parks. `specs`/`system_prompt` are resolved
/// once by [`super::turn::run_round`] for the whole round, not re-derived per
/// attempt.
pub(super) async fn run_attempt(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    setup: &RoundSetup<'_>,
) -> RoundAttempt {
    let RoundSetup {
        specs,
        system_prompt,
        cfg,
        max_turns,
    } = *setup;

    // Bound the inner LLM→tool loop (#177). Each attempt is one LLM
    // round-trip that may fan out into tool calls; a model wedged in a tool
    // loop (or persistently ambiguous, see below) would otherwise run
    // forever. The counter lives on `TurnState`, reset per prompt — a
    // legitimate long session (many prompts) is never capped, only a single
    // runaway turn. User-configurable via `max_turns` (default 200); an
    // ambiguous-stop retry still consumes this budget too, so it remains the
    // hard outer backstop regardless of `max_ambiguous_stop_retries`.
    let turn = s.turn.get_or_insert_with(TurnState::default);
    turn.iterations += 1;
    if turn.iterations > max_turns {
        let _ = events.send(OutEvent::Error {
            session: session.clone(),
            seq: next_seq(&s.seq),
            message: format!("exceeded maximum turn limit ({max_turns}) - possible infinite loop"),
        });
        return RoundAttempt::TurnEnded;
    }

    // Fold any user prompts that arrived mid-turn into the live context
    // before the next model request (#182). This is steering: guidance sent
    // while the turn is running reaches the model on the very next
    // round-trip — the same way a queued user message folds into the next
    // request — instead of replaying as a separate turn after `Done`. Reached
    // on every attempt, including an ambiguous-stop retry, so a prompt sent
    // while the model is being nudged to finish still lands before the retry
    // streams. Non-`Prompt` commands (`SetAgent`) stay stashed for the
    // session loop to handle once this turn ends.
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

    let (text_buf, tool_calls, finish) =
        match stream_round(session, rx, s, events, stash, specs, system_prompt).await {
            StreamedRound::Complete {
                text,
                tool_calls,
                finish,
            } => (text, tool_calls, finish),
            // Stop / inbox close mid-stream — the turn ends but the session
            // stays alive (cancel semantics, ADR-0017).
            StreamedRound::Cancelled => return RoundAttempt::Cancelled,
            // Partial committed + Error/Done already emitted (#181).
            StreamedRound::Failed => return RoundAttempt::TurnEnded,
        };

    // Fold this round-trip's usage into the session total and emit the delta
    // (#192). A `max_tokens`-truncated reply is surfaced as a recoverable
    // warning so it no longer commits silently as a clean turn.
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

    // Classify the stop up front so the commit below can tell an ambiguous
    // round (retried) from a confident one (ends the turn) or a tool round
    // (ADR-0118; classification itself lives on `StopReason::is_confident_stop`,
    // an exhaustive match in entanglement-provider so a new variant forces an
    // explicit classification decision, #433).
    let confident = stop_reason.is_some_and(StopReason::is_confident_stop);
    let ambiguous = tool_calls.is_empty() && !confident;

    // Don't commit an *empty* assistant message on an ambiguous round: a
    // stream that died before emitting any text (the motivating Ollama case)
    // would otherwise push `content: []`, which the strict clients drop
    // entirely (`anthropic.rs`/`gemini` skip a block-less assistant) —
    // leaving the retry request with two adjacent user turns the provider
    // rejects with a 400 (ADR-0118). Replay mirrors this: an empty round logs
    // no `TextDelta`, so its `AmbiguousRetry` fold flushes nothing.
    // (The strict clients also coalesce the resulting adjacent user turns —
    // the original prompt + the nudge — for the same reason.)
    if !(ambiguous && text_buf.is_empty()) {
        s.ctx.push_assistant(text_buf.clone(), tool_calls.clone());
    }
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
        // external resolver) answers each `ToolExec` with `InMsg::ToolResult`;
        // core makes no policy call (#59). Calls execute concurrently —
        // results resolve in any order against the pending set (ADR-0061;
        // deliberate change from the serial in-call-order dispatch this
        // replaced).
        for call in &tool_calls {
            emit_tool_call(events, session, &call.id, &call.name, &call.input, &s.seq);
            emit_tool_exec(events, session, call, &s.profile.name, &s.seq);
        }
        if let Some(turn) = s.turn.as_mut() {
            turn.begin_batch(tool_calls);
        }
        return RoundAttempt::Parked;
    }

    if confident {
        tracing::debug!("no tool calls, confident stop - emitting Done");
        emit_turn_done(session, &s.seq, events);
        return RoundAttempt::TurnEnded;
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
        // pre-ADR-0118 behavior of silently committing the reply, so the very
        // first ambiguous stop must *not* surface a warning it never asked
        // for. Only emit the warning when at least one retry was budgeted
        // (and thus actually attempted).
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
        emit_turn_done(session, &s.seq, events);
        return RoundAttempt::TurnEnded;
    }
    turn.ambiguous_retries += 1;
    tracing::debug!(
        ?stop_reason,
        retries = turn.ambiguous_retries,
        "ambiguous stop - nudging and retrying"
    );
    // Persist the retry as a seq-bearing content event *before* mutating the
    // context (ADR-0118): the bare `push_user` below is invisible to both the
    // persistence tap and the wire, so without this event replay would fold
    // every retry round's `TextDelta`s into one assistant message and lose
    // the nudge — resuming from a history the live model never saw — and
    // heads would concatenate the re-streamed partial text. `Session::replay`
    // folds this by flushing the partial assistant round then pushing
    // `nudge`, reconstructing the exact live boundary.
    let _ = events.send(OutEvent::AmbiguousRetry {
        session: session.clone(),
        seq: next_seq(&s.seq),
        nudge: AMBIGUOUS_STOP_NUDGE.to_string(),
    });
    s.ctx.push_user(AMBIGUOUS_STOP_NUDGE);
    RoundAttempt::AmbiguousRetry
}
