//! Regression tests for the turn-loop stash discipline (ADR-0018).
//!
//! Commands arriving on the session inbox *during* a turn (mid-stream or
//! between tool calls) must be stashed and replayed after the turn ends, not
//! silently dropped. Before ADR-0018, the `try_recv` polls only matched
//! `SessionCmd::Stop` and discarded everything else — so a `Prompt` sent
//! while the engine was mid-turn vanished without trace, and the user's
//! follow-up question was lost.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmSession,
    LlmStream, OutEvent, SessionId,
};

/// Collect `TextDelta` texts for `sid` until the deadline, across as many
/// turns as happen. (Unlike `actor.rs::collect`, this does *not* break on
/// `Done` — we want to see follow-on turns.)
async fn collect_texts_for(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dur: Duration,
) -> Vec<String> {
    let mut texts = Vec::new();
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        let Ok(recv) = tokio::time::timeout_at(deadline, sub.recv()).await else {
            break;
        };
        match recv {
            Ok(OutEvent::TextDelta { text, session, .. }) if session == *sid => {
                texts.push(text);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    texts
}

/// `ScriptedLlm` variant that sleeps before each `stream()` call returns, so
/// a test can reliably inject inbox commands during the streaming window
/// (the `try_recv` polls run inside the consumer loop, which only starts once
/// `stream()` has returned the `LlmStream`).
struct SlowScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
    delay: Duration,
}

impl SlowScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>, delay: Duration) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
            delay,
        }
    }
}

#[async_trait]
impl Llm for SlowScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(self.delay).await;
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

/// Regression: a `Prompt` arriving while the engine is mid-stream must be
/// stashed and replayed once the in-flight turn ends, not dropped.
#[tokio::test]
async fn prompt_arriving_during_streaming_is_stashed_and_replayed() {
    let delay = Duration::from_millis(100);
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "first-reply".into(),
            tool_calls: vec![],
        },
        LlmResponse {
            text: "second-reply".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(SlowScriptedLlm::new((*scripted).clone(), delay)))
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first".into(),
        })
        .await
        .unwrap();

    // Wait until the first turn is inside stream() (which sleeps for `delay`).
    // Sending the second Prompt during this window means it lands in the
    // inbox before the streaming consumer's first try_recv poll.
    tokio::time::sleep(Duration::from_millis(20)).await;
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "second".into(),
        })
        .await
        .unwrap();

    let texts = collect_texts_for(sub, &sid, Duration::from_secs(2)).await;
    assert!(
        texts.iter().any(|t| t == "first-reply"),
        "first turn should produce its reply; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "second-reply"),
        "stashed Prompt must be replayed after the first turn ends; got {texts:?}"
    );
}

/// Regression: a `SetAgent` arriving between tool calls is stashed and
/// applied after the turn. (Any non-Stop command exercises the same stash
/// path; `SetAgent` is a convenient one because its effect — switching the
/// profile — is observable on the next turn.)
#[tokio::test]
async fn setagent_arriving_between_tool_calls_is_stashed_and_applied() {
    // First turn: a tool call (no preamble) so the engine enters the
    // tool-dispatch loop where the second try_recv site lives. The tool is
    // unknown to the registry, which surfaces as a ToolOutput string — the
    // turn completes normally and the stashed SetAgent is then applied.
    let delay = Duration::from_millis(100);
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![entanglement_core::ToolCall {
                id: "t1".into(),
                name: "unknown-tool".into(),
                input: "{}".into(),
            }],
        },
        // Second turn: just text, so we can assert it lands.
        LlmResponse {
            text: "post-setagent-reply".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(SlowScriptedLlm::new((*scripted).clone(), delay)))
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "first".into(),
        })
        .await
        .unwrap();
    // Inject SetAgent mid-turn. Before ADR-0018 this was silently dropped;
    // the next Prompt would still run under the `build` profile.
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Subscribe BEFORE sending SetAgent so we don't miss the AgentChanged
    // event when the engine replays the stashed command after turn 1 ends.
    let mut sub2 = holly.subscribe();
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();

    // Watch for the AgentChanged event (fires when the stashed SetAgent is
    // replayed after turn 1 completes).
    let mut saw_plan = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub2.recv()).await {
        if let OutEvent::AgentChanged { agent, session, .. } = ev {
            if session == sid && agent == "plan" {
                saw_plan = true;
                break;
            }
        }
    }
    assert!(
        saw_plan,
        "stashed SetAgent should have switched the session to the plan profile"
    );

    // Now send a real follow-up Prompt; it runs on the still-alive session.
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "second".into(),
        })
        .await
        .unwrap();

    // And confirm the second turn's reply also surfaced via the original sub.
    let texts = collect_texts_for(sub, &sid, Duration::from_millis(500)).await;
    assert!(
        texts.iter().any(|t| t == "post-setagent-reply"),
        "second turn (post-stash-replay) should produce its reply; got {texts:?}"
    );
}
