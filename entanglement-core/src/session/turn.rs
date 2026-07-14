//! The live reasoning turn: assemble the advertised tool set, stream the LLM
//! response, and drive tool calls to completion. Separable from the replay
//! fold (pure state reconstruction) in `session/replay.rs`.

use std::collections::{HashMap, VecDeque};

use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_turn_error, emit_usage, next_seq};
use super::stream::{stream_round, StreamedRound};
use super::tools::handle_tool_call;
use super::{Session, SessionCmd};
use crate::protocol::{AgentState, OutEvent, SessionId};
use entanglement_provider::{ModelPricing, StopReason, ToolSpec};

/// Runs one reasoning turn to completion. Returns `Err(())` only when a
/// `SessionCmd::Stop` arrives during tool-request approval (cancel-via-Esc);
/// the caller keeps the session task alive and just awaits the next command
/// (ADR-0017). Context is preserved in either case.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    tool_specs: &[ToolSpec],
    profile_tool_specs: &HashMap<String, Vec<ToolSpec>>,
    default_model: Option<&str>,
    pricing: &HashMap<String, ModelPricing>,
) -> Result<(), ()> {
    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });

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
    let mut specs: Vec<ToolSpec> = tool_specs
        .iter()
        .filter(|spec| s.profile.advertises_tool(&spec.name))
        .cloned()
        .collect();
    // Per-profile specs (#119, ADR-0040): the active profile's spawnable roster
    // (the `agent_*` family with a target enum scoped to who *this* profile may
    // spawn) plus the plan-authorship tools (#231) live outside the shared
    // `tool_specs` so a masked schema never reaches the model. The runtime leaves
    // the entry empty for a profile that may not spawn / does not author plans.
    // Still filtered through the #116 mask, so a `disallowed_tools` list can
    // subtract even a per-profile tool.
    if let Some(profile_specs) = profile_tool_specs.get(&s.profile.name) {
        specs.extend(
            profile_specs
                .iter()
                .filter(|spec| s.profile.advertises_tool(&spec.name))
                .cloned(),
        );
    }

    // Bound the inner LLM→tool loop, reset per prompt (#177). Each iteration is
    // one LLM round-trip that may fan out into tool calls; a model wedged in a
    // tool loop would otherwise run forever. Local to this call — a legitimate
    // long session (many prompts) is never capped, only a single runaway turn.
    const MAX_TURNS: usize = 50;
    let mut iterations: usize = 0;
    loop {
        iterations += 1;
        if iterations > MAX_TURNS {
            let _ = events.send(OutEvent::Error {
                session: session.clone(),
                seq: next_seq(&mut s.seq),
                message: format!(
                    "exceeded maximum turn limit ({MAX_TURNS}) - possible infinite loop"
                ),
            });
            return Ok(());
        }

        // Fold any user prompts that arrived mid-turn into the live context
        // before the next model request (#182). This is steering: guidance sent
        // while the turn is running reaches the model on the very next
        // inner-loop round-trip — the same way a queued user message folds into
        // the next request — instead of replaying as a separate turn after
        // `Done`. Only reachable when the previous round emitted tool calls (a
        // reply with none ends the turn below), so a prompt sent after the
        // model's final answer still correctly starts a fresh turn via the
        // stash. Non-`Prompt` commands (`SetAgent`, a stale `ToolResult`) stay
        // stashed for the session loop to handle once this turn ends.
        let mut i = 0;
        while i < stash.len() {
            if matches!(stash[i], SessionCmd::Prompt(_)) {
                if let Some(SessionCmd::Prompt(text)) = stash.remove(i) {
                    tracing::debug!("folding mid-turn prompt into live context");
                    s.ctx.push_user(text);
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
                    &mut s.seq,
                    events,
                    format!(
                        "context window exceeded: {after} tokens estimated after \
                         compaction, over the {}-token budget — start a new session \
                         or shorten the request",
                        s.ctx.limit()
                    ),
                );
                return Ok(());
            }
        }

        let (text_buf, tool_calls, finish) =
            match stream_round(session, rx, s, events, stash, &specs).await {
                StreamedRound::Complete {
                    text,
                    tool_calls,
                    finish,
                } => (text, tool_calls, finish),
                // Cancelled: Stop / inbox close mid-stream — the turn ends but
                // the session stays alive (cancel semantics, ADR-0017).
                // Failed: partial committed + Error/Done already emitted (#181).
                StreamedRound::Cancelled | StreamedRound::Failed => return Ok(()),
            };

        // Fold this round-trip's usage into the session total and emit the delta
        // (#192). A `max_tokens`-truncated reply is surfaced as a recoverable
        // warning so it no longer commits silently as a clean turn.
        if let Some((stop_reason, usage)) = finish {
            let model = s.profile.model.as_deref().or(default_model);
            let cost = model
                .and_then(|m| pricing.get(m))
                .map(|p| p.cost_usd(&usage));
            emit_usage(session, s, events, &usage, cost);
            if stop_reason == Some(StopReason::MaxTokens) {
                let _ = events.send(OutEvent::Error {
                    session: session.clone(),
                    seq: next_seq(&mut s.seq),
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
            "assistant message pushed"
        );
        tracing::debug!(
            context_messages = s.ctx.messages().len(),
            "context after assistant message"
        );

        // End turn if no tool calls (conversation complete)
        if tool_calls.is_empty() {
            tracing::debug!("no tool calls - emitting Done");
            let _ = events.send(OutEvent::Done {
                session: session.clone(),
                seq: next_seq(&mut s.seq),
            });
            let _ = events.send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::Done,
            });
            return Ok(());
        }

        // Execute tool calls
        for call in tool_calls {
            // Drain any commands queued between tools: Stop interrupts, the
            // rest are stashed for replay (ADR-0018).
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    SessionCmd::Stop => {
                        tracing::debug!("turn interrupted between tool calls");
                        let _ = events.send(OutEvent::Status {
                            session: session.clone(),
                            state: AgentState::Idle,
                        });
                        return Ok(());
                    }
                    other => {
                        tracing::debug!(
                            cmd = ?other,
                            "command arrived between tool calls; stashed for replay after turn"
                        );
                        stash.push_back(other);
                    }
                }
            }
            if handle_tool_call(session, rx, s, events, stash, call).await {
                return Err(()); // cancelled
            }
        }
    }
}
