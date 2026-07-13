//! The live reasoning turn: assemble the advertised tool set, stream the LLM
//! response, and drive tool calls to completion. Separable from the replay
//! fold (pure state reconstruction) in `session/replay.rs`.

use std::collections::{HashMap, VecDeque};

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_turn_error, emit_usage, next_seq};
use super::tools::handle_tool_call;
use super::{Session, SessionCmd};
use crate::protocol::{AgentState, OutEvent, SessionId};
use entanglement_provider::{
    Llm, LlmEvent, LlmRequest, ModelPricing, StopReason, ToolCall, ToolSpec,
};

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

        // Stream the model's reply, re-requesting once if the stream fails
        // *before* any user-visible output (#181). The provider retries only
        // connect-level failures and 429s (ADR-0050); a stream that drops after
        // the first byte is invisible to it. A transparent re-request is safe
        // only while nothing has been shown — once a `TextDelta`/`ReasoningDelta`
        // is on screen we cannot silently re-stream over it, so a later failure
        // instead commits the partial with an `[interrupted]` marker (below) to
        // keep the context we send next turn aligned with what the user saw.
        const STREAM_RETRIES: usize = 1;
        let mut attempt: usize = 0;
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish: Option<(Option<StopReason>, entanglement_provider::Usage)> = None;
        let mut shown = false;
        let stream_err: Option<String>;
        loop {
            let req = LlmRequest {
                system: &s.profile.system_prompt,
                model: s.profile.model.as_deref(),
                messages: s.ctx.messages(),
                tools: &specs,
            };
            tracing::debug!(
                messages_count = req.messages.len(),
                estimated_tokens = s.ctx.estimated_tokens(),
                attempt,
                "sending request to LLM"
            );
            let mut stream = match s.llm.stream(req).await {
                Ok(st) => st,
                Err(e) => {
                    emit_turn_error(session, &mut s.seq, events, e.to_string());
                    return Ok(());
                }
            };

            // Consume the stream: emit incremental TextDelta, assemble tool calls.
            let mut attempt_err: Option<String> = None;
            loop {
                // Race the stream against the inbox so a mid-stream command
                // preempts immediately (#179): a stalled-but-connected provider
                // would otherwise block cancellation until the HTTP client's read
                // timeout fires. `biased` polls the inbox first so a queued `Stop`
                // wins even when the stream also has an event ready; dropping the
                // stream aborts the underlying reqwest request. Non-Stop commands
                // are stashed for replay after this turn ends (ADR-0018 —
                // previously silently dropped).
                let ev = tokio::select! {
                    biased;
                    cmd = rx.recv() => {
                        match cmd {
                            Some(SessionCmd::Stop) => {
                                tracing::debug!("turn interrupted during streaming");
                                drop(stream);
                                let _ = events.send(OutEvent::Status {
                                    session: session.clone(),
                                    state: AgentState::Idle,
                                });
                                return Ok(());
                            }
                            // Inbox closed (supervisor gone): abort the stream and
                            // end the turn; the session loop ends on its next recv.
                            None => {
                                drop(stream);
                                return Ok(());
                            }
                            Some(other) => {
                                tracing::debug!(
                                    cmd = ?other,
                                    "command arrived mid-stream; stashed for replay after turn"
                                );
                                stash.push_back(other);
                                continue;
                            }
                        }
                    }
                    next = stream.next() => match next {
                        Some(ev) => ev,
                        None => break,
                    },
                };
                match ev {
                    Ok(LlmEvent::Text(delta)) => {
                        if !delta.is_empty() {
                            shown = true;
                            text_buf.push_str(&delta);
                            let _ = events.send(OutEvent::TextDelta {
                                session: session.clone(),
                                seq: next_seq(&mut s.seq),
                                text: delta,
                            });
                        }
                    }
                    Ok(LlmEvent::Reasoning(delta)) => {
                        if !delta.is_empty() {
                            shown = true;
                            let _ = events.send(OutEvent::ReasoningDelta {
                                session: session.clone(),
                                seq: next_seq(&mut s.seq),
                                text: delta,
                            });
                        }
                    }
                    Ok(LlmEvent::ToolCall(call)) => tool_calls.push(call),
                    Ok(LlmEvent::Finish { stop_reason, usage }) => {
                        finish = Some((stop_reason, usage))
                    }
                    Err(e) => {
                        attempt_err = Some(e.to_string());
                        break;
                    }
                }
            }
            drop(stream);

            match attempt_err {
                None => {
                    stream_err = None;
                    break;
                }
                Some(e) => {
                    if !shown && attempt < STREAM_RETRIES {
                        attempt += 1;
                        // Nothing was shown, so re-request from a clean slate.
                        text_buf.clear();
                        tool_calls.clear();
                        finish = None;
                        tracing::warn!(
                            error = %e,
                            attempt,
                            "stream failed before any output; re-requesting turn"
                        );
                        continue;
                    }
                    stream_err = Some(e);
                    break;
                }
            }
        }

        if let Some(msg) = stream_err {
            // A mid-stream failure after partial output. Commit the partial with
            // an `[interrupted]` marker so the context we send next turn matches
            // what the user saw (#181) — otherwise the model continues as if it
            // had said nothing. Stream the marker too, so display and context stay
            // identical. Any half-assembled tool calls are dropped: without the
            // `Finish` they may be incomplete and unsafe to execute.
            if !text_buf.is_empty() {
                const MARKER: &str = "\n\n[interrupted]";
                let _ = events.send(OutEvent::TextDelta {
                    session: session.clone(),
                    seq: next_seq(&mut s.seq),
                    text: MARKER.to_string(),
                });
                text_buf.push_str(MARKER);
                s.ctx.push_assistant(text_buf, Vec::new());
            }
            emit_turn_error(session, &mut s.seq, events, msg);
            return Ok(());
        }

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
