//! Integration tests for usage/cost surfacing (#192): the engine folds the
//! provider's `LlmEvent::Finish` metadata into an `OutEvent::Usage` (with a
//! priced `cost_usd`) and surfaces a `max_tokens`-truncated reply as a
//! recoverable warning instead of committing it as a clean turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmSession, LlmStream, ModelPricing,
    OutEvent, SessionId, StopReason, Usage,
};
use futures::stream;
use futures::StreamExt;

/// An `Llm` that streams one text chunk then a `Finish` carrying a scripted
/// stop reason + usage — enough to exercise the engine's fold/emit path without
/// a real provider.
struct FinishLlm {
    stop_reason: Option<StopReason>,
    usage: Usage,
}

#[async_trait]
impl Llm for FinishLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let events = vec![
            Ok(LlmEvent::Text("done".into())),
            Ok(LlmEvent::Finish {
                stop_reason: self.stop_reason,
                usage: self.usage,
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

fn config(
    stop_reason: Option<StopReason>,
    usage: Usage,
    pricing: Option<ModelPricing>,
) -> EngineConfig {
    let mut prices = HashMap::new();
    if let Some(p) = pricing {
        prices.insert("test-model".to_string(), p);
    }
    EngineConfig {
        llm_factory: Arc::new(move || LlmSession::new(Box::new(FinishLlm { stop_reason, usage }))),
        default_model: Some("test-model".to_string()),
        pricing: prices,
        ..EngineConfig::default()
    }
}

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

async fn run(cfg: EngineConfig) -> Vec<OutEvent> {
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    collect(sub, &sid).await
}

#[tokio::test]
async fn finish_usage_is_emitted_with_priced_cost() {
    // $1/M input, $2/M output, $0.5/M cached read: 70*1 + 40*2 + 30*0.5 per M.
    let pricing = ModelPricing {
        input: Some(1.0),
        output: Some(2.0),
        cached_input: Some(0.5),
        cache_write: None,
    };
    let usage = Usage {
        input_tokens: Some(70),
        output_tokens: Some(40),
        cached_input_tokens: Some(30),
        cache_write_tokens: None,
    };
    let events = run(config(Some(StopReason::EndTurn), usage, Some(pricing))).await;

    let (input, output, cached, cost) = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cost_usd,
                ..
            } => Some((
                *input_tokens,
                *output_tokens,
                *cached_input_tokens,
                *cost_usd,
            )),
            _ => None,
        })
        .expect("a Usage event");
    assert_eq!((input, output, cached), (70, 40, 30));
    let cost = cost.expect("priced model yields a cost");
    let expected = (70.0 * 1.0 + 40.0 * 2.0 + 30.0 * 0.5) / 1_000_000.0;
    assert!((cost - expected).abs() < 1e-12, "cost {cost} != {expected}");

    // A clean stop reason emits no truncation warning.
    assert!(!events.iter().any(|e| matches!(e, OutEvent::Error { .. })));
}

#[tokio::test]
async fn usage_cost_is_none_without_catalog_pricing() {
    let usage = Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        ..Usage::default()
    };
    let events = run(config(Some(StopReason::EndTurn), usage, None)).await;

    let cost = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Usage { cost_usd, .. } => Some(*cost_usd),
            _ => None,
        })
        .expect("a Usage event");
    assert_eq!(cost, None, "unpriced model must report cost_usd = None");
}

#[tokio::test]
async fn max_tokens_stop_reason_surfaces_a_warning() {
    let events = run(config(Some(StopReason::MaxTokens), Usage::default(), None)).await;

    // The truncated reply still commits (Done), but a recoverable Error warns.
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::Error { message, .. } if message.contains("max_tokens")
        )),
        "max_tokens must surface a truncation warning: {events:?}"
    );
}
