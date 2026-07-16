//! Live generation-parameter changes (#374, ADR-0094): an `InMsg::SetGeneration`
//! merges partial overrides onto the session's current generation, always emits
//! `OutEvent::GenerationChanged` with the full merged result, and the merge
//! reaches the next `LlmRequest`. Deferred (stashed) while a turn is live, like
//! `SetAgent`/`SetModel`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, GenerationParams, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, OutEvent, ReasoningEffort, SessionId,
};

/// Every request's effective generation knobs, in order.
type Seen = Arc<Mutex<Vec<Option<GenerationParams>>>>;

struct RecordingLlm {
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.generation);
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

fn recording_factory(seen: &Seen) -> entanglement_core::LlmFactory {
    let seen = seen.clone();
    Arc::new(move || Box::new(RecordingLlm { seen: seen.clone() }) as Box<dyn Llm>)
}

async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

fn is_generation_changed(e: &OutEvent) -> bool {
    matches!(e, OutEvent::GenerationChanged { .. })
}

#[tokio::test]
async fn overrides_merge_onto_the_current_generation_and_reach_the_request() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = EngineConfig {
        llm_factory: recording_factory(&seen),
        generation: Some(GenerationParams {
            temperature: Some(0.2),
            max_output_tokens: Some(1024),
            thinking_budget_tokens: None,
            reasoning_effort: None,
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // Only `temperature` + `reasoning_effort` are overridden; `max_output_tokens`
    // must survive untouched.
    holly
        .send(InMsg::SetGeneration {
            session: sid.clone(),
            overrides: GenerationParams {
                temperature: Some(0.9),
                max_output_tokens: None,
                thinking_budget_tokens: None,
                reasoning_effort: Some(ReasoningEffort::High),
            },
        })
        .await
        .unwrap();

    let ev = recv_until(&mut sub, is_generation_changed).await;
    let OutEvent::GenerationChanged { generation, .. } = ev else {
        unreachable!()
    };
    assert_eq!(generation.temperature, Some(0.9));
    assert_eq!(generation.max_output_tokens, Some(1024));
    assert_eq!(generation.thinking_budget_tokens, None);
    assert_eq!(generation.reasoning_effort, Some(ReasoningEffort::High));

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    recv_until(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests, vec![Some(generation)]);
}

#[tokio::test]
async fn empty_overrides_still_emit_generation_changed() {
    // A direct `SetGeneration` always confirms the write via `GenerationChanged`
    // — even when every field happens to already match — so a head can rely on
    // the reply rather than guessing whether anything changed.
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = EngineConfig {
        llm_factory: recording_factory(&seen),
        generation: Some(GenerationParams {
            temperature: Some(0.5),
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: None,
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::SetGeneration {
            session: sid.clone(),
            overrides: GenerationParams::default(),
        })
        .await
        .unwrap();

    let ev = recv_until(&mut sub, is_generation_changed).await;
    let OutEvent::GenerationChanged { generation, .. } = ev else {
        unreachable!()
    };
    assert_eq!(generation.temperature, Some(0.5));
}

/// An `Llm` that sleeps before returning, so a mid-turn `SetGeneration` reliably
/// lands in the inbox before the turn's stream resolves.
struct SlowLlm {
    delay: Duration,
}

#[async_trait]
impl Llm for SlowLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(self.delay).await;
        Ok(stream_from_response(LlmResponse {
            text: "turn reply".into(),
            tool_calls: vec![],
        }))
    }
}

#[tokio::test]
async fn set_generation_during_a_live_turn_is_deferred_until_it_ends() {
    let delay = Duration::from_millis(150);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || Box::new(SlowLlm { delay }) as Box<dyn Llm>),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    // Land inside the streaming delay window, before the turn resolves.
    tokio::time::sleep(Duration::from_millis(20)).await;
    holly
        .send(InMsg::SetGeneration {
            session: sid.clone(),
            overrides: GenerationParams {
                temperature: Some(0.1),
                ..GenerationParams::default()
            },
        })
        .await
        .unwrap();

    // The live turn's own Done must land before GenerationChanged — the
    // SetGeneration was stashed, not applied concurrently.
    let mut events: VecDeque<OutEvent> = VecDeque::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out")
            .expect("event stream closed");
        let is_done = matches!(ev, OutEvent::Done { .. });
        events.push_back(ev);
        if is_done {
            break;
        }
    }
    assert!(
        !events.iter().any(is_generation_changed),
        "GenerationChanged must not land before the live turn's Done: {events:?}"
    );

    // The stashed command applies once the turn ends.
    recv_until(&mut sub, is_generation_changed).await;
}
