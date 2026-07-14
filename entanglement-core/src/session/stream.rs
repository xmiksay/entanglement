//! One streamed LLM round-trip: send the request, race the stream against the
//! session inbox, emit incremental deltas, and assemble the reply. Split out of
//! the turn loop (#269) so the loop reads as control flow over completed
//! rounds; all mid-stream policy (#179 preemption, #181 retry/partial-commit)
//! lives here.

use std::collections::VecDeque;

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use super::emit::{emit_turn_error, next_seq};
use super::{Session, SessionCmd};
use crate::protocol::{AgentState, OutEvent, SessionId};
use entanglement_provider::{
    GenerationParams, LlmEvent, LlmRequest, StopReason, ToolCall, ToolSpec, Usage,
};

/// Outcome of one streamed round-trip.
pub(super) enum StreamedRound {
    /// Stream completed: committed-ready text, assembled calls, `Finish` payload.
    Complete {
        text: String,
        tool_calls: Vec<ToolCall>,
        finish: Option<(Option<StopReason>, Usage)>,
    },
    /// `Stop` / inbox close arrived mid-stream (`Idle` already emitted for the
    /// `Stop` case, matching cancel semantics — ADR-0017/#179).
    Cancelled,
    /// Terminal stream error: the partial (if any) is already committed with an
    /// `[interrupted]` marker and `emit_turn_error` has fired (#181).
    Failed,
}

/// Stream the model's reply, re-requesting once if the stream fails *before*
/// any user-visible output (#181). The provider retries only connect-level
/// failures and 429s (ADR-0050); a stream that drops after the first byte is
/// invisible to it. A transparent re-request is safe only while nothing has
/// been shown — once a `TextDelta`/`ReasoningDelta` is on screen we cannot
/// silently re-stream over it, so a later failure instead commits the partial
/// with an `[interrupted]` marker to keep the context we send next turn
/// aligned with what the user saw.
pub(super) async fn stream_round(
    session: &SessionId,
    rx: &mut mpsc::Receiver<SessionCmd>,
    s: &mut Session,
    events: &broadcast::Sender<OutEvent>,
    stash: &mut VecDeque<SessionCmd>,
    specs: &[ToolSpec],
    generation: Option<GenerationParams>,
) -> StreamedRound {
    const STREAM_RETRIES: usize = 1;
    let mut attempt: usize = 0;
    let mut text_buf = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut finish: Option<(Option<StopReason>, Usage)> = None;
    let mut shown = false;
    let stream_err: Option<String>;
    loop {
        let req = LlmRequest {
            system: &s.profile.system_prompt,
            model: s.profile.model.as_deref(),
            messages: s.ctx.messages(),
            tools: specs,
            generation,
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
                return StreamedRound::Failed;
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
                            return StreamedRound::Cancelled;
                        }
                        // Inbox closed (supervisor gone): abort the stream and
                        // end the turn; the session loop ends on its next recv.
                        None => {
                            drop(stream);
                            return StreamedRound::Cancelled;
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
                Ok(LlmEvent::Finish { stop_reason, usage }) => finish = Some((stop_reason, usage)),
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
        return StreamedRound::Failed;
    }

    StreamedRound::Complete {
        text: text_buf,
        tool_calls,
        finish,
    }
}
