//! The live reasoning turn: assemble the advertised tool set, stream the LLM
//! response, and drive tool calls to completion. Separable from the replay
//! fold (pure state reconstruction) in `session/replay.rs`.

use std::collections::{HashMap, VecDeque};

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_turn_error, next_seq};
use super::tools::handle_tool_call;
use super::{Session, SessionCmd, PLAN_TOOL, TASKS_TOOL};
use crate::llm::{Llm, LlmEvent, LlmRequest, ToolCall, ToolSpec};
use crate::protocol::{AgentState, OutEvent, SessionId};

/// Runs one reasoning turn to completion. Returns `Err(())` only when a
/// `SessionCmd::Stop` arrives during tool-request approval (cancel-via-Esc);
/// the caller keeps the session task alive and just awaits the next command
/// (ADR-0017). Context is preserved in either case.
pub(crate) async fn run_turn(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    tool_specs: &[ToolSpec],
    profile_tool_specs: &HashMap<String, Vec<ToolSpec>>,
) -> Result<(), ()> {
    s.turn_count += 1;
    const MAX_TURNS: usize = 50;
    if s.turn_count > MAX_TURNS {
        let _ = events.send(OutEvent::Error {
            session: session.clone(),
            seq: next_seq(&mut s.seq),
            message: format!("exceeded maximum turn limit ({MAX_TURNS}) - possible infinite loop"),
        });
        return Ok(());
    }

    let _ = events.send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });

    // Tool set advertised to the model = host tools (from config, #61) filtered
    // by the active profile's allowlist/denylist mask (#116, ADR-0038), plus the
    // two built-ins. Core caches no fixed tool set on the session; the schemas
    // come from `EngineConfig.tool_specs` at turn time. The mask is a *physical*
    // restriction — a masked tool's schema never reaches the model — layered
    // under the runtime's `Allow`/`Ask`/`Deny` dispatch, which grades only the
    // tools that survive here. The `update_plan`/`update_tasks` built-ins below
    // are session-state tools, not host tools, so they bypass the mask.
    //
    // Plan authority is separate and default-closed (#140, ADR-0041): the
    // `update_plan` spec is advertised *only* to a profile that `owns_plan`;
    // `update_tasks` stays unconditional (per-session bookkeeping, no
    // cross-agent authority). This lives in core because the built-ins never
    // round-trip to the runtime, so the runtime's `tool_masked` gate can't see
    // them.
    let mut specs: Vec<ToolSpec> = tool_specs
        .iter()
        .filter(|spec| s.profile.advertises_tool(&spec.name))
        .cloned()
        .collect();
    // Per-profile specs (#119, ADR-0040): the active profile's spawnable roster
    // (the `agent_*` family with a target enum scoped to who *this* profile may
    // spawn) lives outside the shared `tool_specs` so a masked schema never
    // reaches the model. The runtime leaves the entry empty for a profile that
    // may not spawn. Still filtered through the #116 mask, so a `disallowed_tools`
    // list can subtract even a per-profile tool.
    if let Some(profile_specs) = profile_tool_specs.get(&s.profile.name) {
        specs.extend(
            profile_specs
                .iter()
                .filter(|spec| s.profile.advertises_tool(&spec.name))
                .cloned(),
        );
    }
    if s.profile.owns_plan {
        specs.push(ToolSpec::with_schema(
            PLAN_TOOL,
            "Replace the strategy plan (markdown prose).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The full plan document, in markdown."
                    }
                },
                "required": ["content"]
            }),
        ));
    }
    specs.push(ToolSpec::with_schema(
        TASKS_TOOL,
        "Replace the task list (markdown). Shown to the user as progress info — \
         it is not fed back to you, so keep it a short checklist.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full task list, in markdown — e.g. `- [ ]` / `- [x]` checkbox lines."
                }
            },
            "required": ["content"]
        }),
    ));

    loop {
        if !s.ctx.within_limit() {
            let _ = events.send(OutEvent::Error {
                session: session.clone(),
                seq: next_seq(&mut s.seq),
                message: format!("context over limit ({} tokens)", s.ctx.estimated_tokens()),
            });
        }

        let req = LlmRequest {
            system: &s.profile.system_prompt,
            model: s.profile.model.as_deref(),
            messages: s.ctx.messages(),
            tools: &specs,
        };
        tracing::debug!(
            messages_count = req.messages.len(),
            estimated_tokens = s.ctx.estimated_tokens(),
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
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut stream_err: Option<String> = None;
        while let Some(ev) = stream.next().await {
            // Drain any commands queued mid-stream: Stop interrupts the turn,
            // everything else is stashed for replay after this turn ends
            // (ADR-0018 — previously non-Stop commands were silently dropped).
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    SessionCmd::Stop => {
                        tracing::debug!("turn interrupted during streaming");
                        drop(stream);
                        let _ = events.send(OutEvent::Status {
                            session: session.clone(),
                            state: AgentState::Idle,
                        });
                        return Ok(());
                    }
                    other => {
                        tracing::debug!(
                            cmd = ?other,
                            "command arrived mid-stream; stashed for replay after turn"
                        );
                        stash.push_back(other);
                    }
                }
            }
            match ev {
                Ok(LlmEvent::Text(delta)) => {
                    if !delta.is_empty() {
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
                        let _ = events.send(OutEvent::ReasoningDelta {
                            session: session.clone(),
                            seq: next_seq(&mut s.seq),
                            text: delta,
                        });
                    }
                }
                Ok(LlmEvent::ToolCall(call)) => tool_calls.push(call),
                Ok(LlmEvent::Finish { .. }) => {}
                Err(e) => {
                    stream_err = Some(e.to_string());
                    break;
                }
            }
        }
        drop(stream);

        if let Some(msg) = stream_err {
            // Partial text was already streamed; do not commit the failed turn.
            emit_turn_error(session, &mut s.seq, events, msg);
            return Ok(());
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
