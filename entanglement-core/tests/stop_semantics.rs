//! Regression tests for `InMsg::Stop` semantics (ADR-0017).
//!
//! `Stop` cancels the in-flight turn but must NOT destroy the session task or
//! its `Context`. A subsequent `Prompt` to the same `SessionId` must continue
//! the conversation — the LLM sees the prior turn's user message in its next
//! request. Both tests below failed before ADR-0017 (Stop evicted the session
//! from the supervisor map; the next `Prompt` lazily spawned a fresh task with
//! empty history).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmSession,
    LlmStream, Message, MessageRole, OutEvent, SessionId, ToolCall,
};
use futures::StreamExt;

/// Collect events for `sid` until `Done`, with a safety timeout.
async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let Ok(recv) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == sid => {
                let done = matches!(ev, OutEvent::Done { .. });
                out.push(ev);
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// ScriptedLlm variant that records every request's `messages` slice into a
/// shared snapshot, so a test can assert what the model was shown across
/// multiple turns.
struct CapturingLlm {
    responses: Mutex<Vec<LlmResponse>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl CapturingLlm {
    fn new(responses: Vec<LlmResponse>, seen: Arc<Mutex<Vec<Vec<Message>>>>) -> Self {
        Self {
            responses: Mutex::new(responses),
            seen,
        }
    }
}

#[async_trait]
impl Llm for CapturingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let resp = {
            let mut responses = self.responses.lock().unwrap();
            responses.pop().unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            })
        };
        Ok(stream_from_response(resp))
    }
}

fn capturing_factory(
    responses: Vec<LlmResponse>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
) -> EngineConfig {
    let mut r = responses;
    r.reverse();
    EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(CapturingLlm::new(r.clone(), seen.clone())))
        }),
        ..EngineConfig::default()
    }
}

/// An `Llm` whose stream connects but stays silent forever — it yields no
/// events and never completes, modelling a provider that stalls mid-response
/// (#179). Records requests into `seen` so a follow-up turn can be asserted.
struct StallingLlm {
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
    stalled_once: Mutex<bool>,
    reply: String,
}

#[async_trait]
impl Llm for StallingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        // The first turn stalls (silent, never-ready stream); a later turn
        // returns a real reply so the re-prompt can complete.
        let first = {
            let mut done = self.stalled_once.lock().unwrap();
            let first = !*done;
            *done = true;
            first
        };
        if first {
            Ok(futures::stream::pending().boxed())
        } else {
            Ok(stream_from_response(LlmResponse {
                text: self.reply.clone(),
                tool_calls: vec![],
            }))
        }
    }
}

/// A silent-but-connected stream must not wedge cancellation: `Stop` has to
/// preempt `stream.next()` immediately (#179). Before the fix, `Stop` was only
/// drained *after* the next stream event, so a stalled stream blocked cancel
/// until the HTTP client's read timeout. This test would hang (fail on the
/// collect timeout) under the old poll-after-yield loop.
#[tokio::test]
async fn stop_preempts_a_stalled_stream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(StallingLlm {
                seen: seen2.clone(),
                stalled_once: Mutex::new(false),
                reply: "recovered".into(),
            }))
        }),
        ..EngineConfig::default()
    });
    let sid = SessionId::new("s1");

    // Turn 1: the model stalls. Watch for Thinking so we know the stream is live.
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first-prompt".into(),
        })
        .await
        .unwrap();
    let mut thinking = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if matches!(&ev, OutEvent::Status { state, .. } if *state == entanglement_core::AgentState::Thinking)
        {
            thinking = true;
            break;
        }
    }
    assert!(thinking, "turn should enter Thinking on the stalled stream");

    // Stop must interrupt the silent stream promptly (well under any HTTP
    // timeout). Expect the Idle status the interrupt path emits.
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let mut went_idle = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if matches!(&ev, OutEvent::Status { state, .. } if *state == entanglement_core::AgentState::Idle)
        {
            went_idle = true;
            break;
        }
    }
    assert!(
        went_idle,
        "Stop must preempt the stalled stream and return the session to Idle"
    );

    // The session task survives the interrupt and answers a fresh prompt.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "second-prompt".into(),
        })
        .await
        .unwrap();
    let events = collect(sub2, &sid).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "recovered")),
        "session should recover and reply after the stalled turn was cancelled; got {events:?}"
    );
}

#[tokio::test]
async fn stop_while_idle_preserves_context_for_next_prompt() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(capturing_factory(
        vec![
            LlmResponse {
                text: "first-reply".into(),
                tool_calls: vec![],
            },
            LlmResponse {
                text: "second-reply".into(),
                tool_calls: vec![],
            },
        ],
        seen.clone(),
    ));
    let sid = SessionId::new("s1");

    // Turn 1: completes normally.
    let sub1 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first-prompt".into(),
        })
        .await
        .unwrap();
    let e1 = collect(sub1, &sid).await;
    assert!(
        e1.iter().any(|e| matches!(e, OutEvent::Done { .. })),
        "turn 1 should complete"
    );

    // Stop arrives while idle (no turn in flight).
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    // Give the session task a moment to process the Stop.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Turn 2: the Llm must see the first user prompt still in history.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "second-prompt".into(),
        })
        .await
        .unwrap();
    let e2 = collect(sub2, &sid).await;
    assert!(
        e2.iter().any(|e| matches!(e, OutEvent::Done { .. })),
        "turn 2 should complete after Stop"
    );

    let snapshots = seen.lock().unwrap().clone();
    assert!(
        !snapshots.is_empty(),
        "capturing Llm should have recorded at least one request"
    );
    let last_messages = snapshots.last().unwrap();
    let user_texts: Vec<&str> = last_messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .map(|m| m.text.as_str())
        .collect();
    assert!(
        user_texts.contains(&"first-prompt"),
        "second turn should still see first-prompt in history (Stop must not destroy Context); got {user_texts:?}"
    );
    assert!(
        user_texts.contains(&"second-prompt"),
        "second turn should see second-prompt; got {user_texts:?}"
    );
}

#[tokio::test]
async fn stop_during_tool_exec_keeps_session_alive() {
    // A tool call parks the engine in `wait_tool_result` (it emitted `ToolExec`
    // and awaits the runtime's `ToolResult`, #58). With no tool executor wired
    // here, that wait never resolves on its own; we send Stop to cancel it (the
    // Esc-in-approval path is the same cancel now that permission dispatch and
    // approval live in the runtime, #59) and re-prompt. The session task must
    // still be alive to handle the new prompt.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(capturing_factory(
        vec![
            // First turn: emit a tool call so the engine parks on the result.
            LlmResponse {
                text: "".into(),
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: "echo hi".into(),
                }],
            },
            // Second turn (after Stop + new Prompt): plain text reply.
            LlmResponse {
                text: "recovered".into(),
                tool_calls: vec![],
            },
        ],
        seen.clone(),
    ));
    let sid = SessionId::new("s1");

    let _sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first-prompt".into(),
        })
        .await
        .unwrap();

    // Wait for the ToolExec (engine is now parked in wait_tool_result).
    let mut sub2 = holly.subscribe();
    let mut got_request = false;
    while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub2.recv()).await {
        if let Ok(OutEvent::ToolExec { .. }) = ev {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "expected ToolExec while the tool call is pending"
    );

    // Cancel the pending tool call with Stop.
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Re-prompt — the session task must still be alive.
    let sub3 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "second-prompt".into(),
        })
        .await
        .unwrap();
    let events = collect(sub3, &sid).await;
    assert!(
        events.iter().any(|e| matches!(e, OutEvent::Done { .. })),
        "session should still respond after Stop cancelled the approval"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "recovered")),
        "session should produce the scripted reply; got {events:?}"
    );

    // And the original prompt is still in history — Stop didn't destroy Context.
    let snapshots = seen.lock().unwrap().clone();
    let last = snapshots.last().unwrap();
    assert!(
        last.iter()
            .any(|m| m.role == MessageRole::User && m.text == "first-prompt"),
        "post-Stop turn should still see the original first-prompt in history"
    );
}
