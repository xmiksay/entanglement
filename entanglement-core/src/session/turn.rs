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
use super::turn_state::TurnState;
use super::{Session, SessionCmd};
use crate::protocol::{AgentState, OutEvent, SessionId};
use crate::EngineConfig;
use entanglement_provider::{StopReason, ToolSpec};

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

    // Bound the inner LLM→tool loop (#177). Each round is one LLM round-trip
    // that may fan out into tool calls; a model wedged in a tool loop would
    // otherwise run forever. The counter lives on `TurnState`, reset per
    // prompt — a legitimate long session (many prompts) is never capped, only
    // a single runaway turn. User-configurable via `max_turns` (default 200).
    let max_turns = cfg.max_turns.max(1);
    let turn = s.turn.get_or_insert_with(TurnState::default);
    turn.iterations += 1;
    if turn.iterations > max_turns {
        let _ = events.send(OutEvent::Error {
            session: session.clone(),
            seq: next_seq(&s.seq),
            message: format!("exceeded maximum turn limit ({max_turns}) - possible infinite loop"),
        });
        return RoundOutcome::TurnEnded;
    }

    // Fold any user prompts that arrived mid-turn into the live context
    // before the next model request (#182). This is steering: guidance sent
    // while the turn is running reaches the model on the very next
    // round-trip — the same way a queued user message folds into the next
    // request — instead of replaying as a separate turn after `Done`. Only
    // reachable when the previous round emitted tool calls (a reply with none
    // ends the turn below), so a prompt sent after the model's final answer
    // still correctly starts a fresh turn via the stash. Non-`Prompt` commands
    // (`SetAgent`) stay stashed for the session loop to handle once this turn
    // ends.
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

    // Keep the request inside the model's real context window (#178). Over
    // budget, first compact (prune the oldest tool outputs); if that still
    // doesn't fit, refuse the turn — sending an over-window request just
    // burns a paid round-trip and errors at the provider. Refusing ends the
    // turn cleanly (Error + Done + Status) so a one-shot head unblocks.
    if !s.ctx.within_limit() {
        let before = s.ctx.estimated_tokens();
        let fits = s.ctx.compact();
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
    // `system_prompt_resolver` is wired (#310, ADR-0078). An embedder whose
    // prompt is user-editable content (a site serving it from a CMS page)
    // consults it here so an edit lands on this turn with no engine respawn; a
    // `None` return falls back to the profile's static prompt. Resolved as an
    // owned `String` up front so `stream_round` borrows nothing extra off `s`.
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
            // Stop / inbox close mid-stream — the turn ends but the session
            // stays alive (cancel semantics, ADR-0017).
            StreamedRound::Cancelled => return RoundOutcome::Cancelled,
            // Partial committed + Error/Done already emitted (#181).
            StreamedRound::Failed => return RoundOutcome::TurnEnded,
        };

    // Fold this round-trip's usage into the session total and emit the delta
    // (#192). A `max_tokens`-truncated reply is surfaced as a recoverable
    // warning so it no longer commits silently as a clean turn.
    if let Some((stop_reason, usage)) = finish {
        // A live model switch (#218) prices the turn under the switched model.
        let model = s
            .model
            .as_deref()
            .or(s.profile.model.as_deref())
            .or(cfg.default_model.as_deref());
        let cost = model
            .and_then(|m| cfg.pricing.get(m))
            .map(|p| p.cost_usd(&usage));
        emit_usage(session, s, events, &usage, cost);
        if stop_reason == Some(StopReason::MaxTokens) {
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

    if tool_calls.is_empty() {
        tracing::debug!("no tool calls - emitting Done");
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

    // Emit the whole batch up front and park (#270). Every tool is a protocol
    // round-trip (#58): the runtime tool executor (or any external resolver)
    // answers each `ToolExec` with `InMsg::ToolResult`; core makes no policy
    // call (#59). Calls execute concurrently — results resolve in any order
    // against the pending set (ADR-0061; deliberate change from the serial
    // in-call-order dispatch this replaced).
    for call in &tool_calls {
        emit_tool_call(events, session, &call.id, &call.name, &call.input, &s.seq);
        emit_tool_exec(events, session, call, &s.profile.name, &s.seq);
    }
    if let Some(turn) = s.turn.as_mut() {
        turn.begin_batch(tool_calls);
    }
    RoundOutcome::Parked
}
