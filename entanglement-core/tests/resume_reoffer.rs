//! Resume of a mid-turn log (#272, ADR-0061): the reconstructed session
//! re-offers its pending `ToolExec`s (same `request_id`, fresh `seq`) so any
//! resolver can answer them, then the turn continues to `Done`. This is the
//! executable proof of the embedder persistence seam: records in, resolution
//! by message, completion out.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentState, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmSession, LlmStream, OutEvent, SessionId,
};

struct ScriptedLlm {
    responses: Vec<LlmResponse>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.responses.pop().unwrap_or_else(|| LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        });
        Ok(stream_from_response(resp))
    }
}

/// Engine whose every session streams the given responses (popped from the
/// back — pass them in reverse order of use).
fn engine(responses: Vec<LlmResponse>) -> Holly {
    let responses = Arc::new(responses);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(ScriptedLlm {
                responses: (*responses).clone(),
            }))
        }),
        ..EngineConfig::default()
    };
    Holly::spawn(cfg)
}

/// A crashed-mid-turn log: prompt, then one tool call offered but never
/// answered.
fn mid_turn_records(sid: &SessionId) -> Vec<(Option<InMsg>, OutEvent)> {
    vec![
        (
            Some(InMsg::Prompt {
                session: sid.clone(),
                text: "read file".into(),
            }),
            OutEvent::Status {
                session: sid.clone(),
                state: AgentState::Thinking,
            },
        ),
        (
            None,
            OutEvent::ToolCall {
                session: sid.clone(),
                seq: 1,
                request_id: "call_1".into(),
                tool: "read".into(),
                input: "{}".into(),
            },
        ),
        (
            None,
            OutEvent::ToolExec {
                session: sid.clone(),
                seq: 2,
                request_id: "call_1".into(),
                tool: "read".into(),
                input: "{}".into(),
            },
        ),
    ]
}

#[tokio::test]
async fn resume_reoffers_pending_calls_and_completes() {
    let holly = engine(vec![LlmResponse {
        text: "final".into(),
        tool_calls: vec![],
    }]);
    let sid = SessionId::new("resume-1");
    let mut sub = holly.subscribe();

    holly
        .resume(sid.clone(), mid_turn_records(&sid))
        .await
        .unwrap();

    // The resumed session must re-offer call_1 as a fresh ToolExec with a seq
    // above the replayed max (2).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let reoffer = loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("re-offer must arrive")
            .expect("channel open");
        if let OutEvent::ToolExec {
            session,
            seq,
            request_id,
            ..
        } = ev
        {
            if session == sid {
                break (seq, request_id);
            }
        }
    };
    assert_eq!(reoffer.1, "call_1", "same request_id as the logged offer");
    assert!(reoffer.0 > 2, "fresh seq above the replayed max");

    // Any resolver answers it by message; the turn continues to Done.
    holly
        .send(InMsg::ToolResult {
            session: sid.clone(),
            request_id: "call_1".into(),
            output: "content".into(),
        })
        .await
        .unwrap();

    let mut saw_final = false;
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !(saw_final && saw_done) {
        let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await else {
            break;
        };
        match ev {
            OutEvent::TextDelta { session, text, .. } if session == sid && text == "final" => {
                saw_final = true;
            }
            OutEvent::Done { session, .. } if session == sid => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_final, "the next round streams after resolution");
    assert!(saw_done, "the resumed turn completes");
}

#[tokio::test]
async fn resume_of_drained_tail_continues_the_turn_without_reoffer() {
    let holly = engine(vec![LlmResponse {
        text: "continued".into(),
        tool_calls: vec![],
    }]);
    let sid = SessionId::new("resume-2");
    let mut sub = holly.subscribe();

    // The crash hit after the result was logged but before the next round.
    let mut records = mid_turn_records(&sid);
    records.push((
        None,
        OutEvent::ToolOutput {
            session: sid.clone(),
            seq: 3,
            request_id: "call_1".into(),
            tool: "read".into(),
            output: "content".into(),
        },
    ));

    holly.resume(sid.clone(), records).await.unwrap();

    let mut saw_reoffer = false;
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !saw_done {
        let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await else {
            break;
        };
        match ev {
            OutEvent::ToolExec { session, .. } if session == sid => saw_reoffer = true,
            OutEvent::Done { session, .. } if session == sid => saw_done = true,
            _ => {}
        }
    }
    assert!(!saw_reoffer, "nothing to re-offer on a drained tail");
    assert!(saw_done, "the turn continues straight to completion");
}
