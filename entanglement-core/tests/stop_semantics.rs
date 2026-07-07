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
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, MessageRole, OutEvent, SessionId, ToolCall,
};

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
            Box::new(CapturingLlm::new(r.clone(), seen.clone())) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    }
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
async fn stop_during_tool_approval_keeps_session_alive() {
    // plan profile → bash Ask. The engine pauses in `wait_approval`; we send
    // Stop to cancel (the Esc-in-approval path) and then re-prompt. The
    // session task must still be alive to handle the new prompt.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(capturing_factory(
        vec![
            // First turn: emit a tool call so the engine enters wait_approval.
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
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();

    let _sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first-prompt".into(),
        })
        .await
        .unwrap();

    // Wait for the ToolRequest (engine is now paused in wait_approval).
    let mut sub2 = holly.subscribe();
    let mut got_request = false;
    while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub2.recv()).await {
        if let Ok(OutEvent::ToolRequest { .. }) = ev {
            got_request = true;
            break;
        }
    }
    assert!(got_request, "expected ToolRequest under plan profile");

    // Esc-in-approval path: send Stop instead of Approve.
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
