//! Integration tests for the extended-thinking round-trip: a captured
//! `LlmEvent::ContentBlock(ContentPart::Reasoning)` must surface as a persisted
//! `OutEvent::ReasoningBlock`, land in the committed assistant `Message`
//! alongside the round's text, and stay distinct from the live-display
//! `ReasoningDelta` channel.
//!
//! Anthropic requires the unmodified thinking block back on the final assistant
//! message whenever tool results are returned — which is exactly a parked turn's
//! shape (ADR-0061) — so a reasoning channel that only streamed for display
//! could not satisfy it. Persistence is what lets a *resumed* session rebuild a
//! history the provider still accepts.

use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::{
    ContentPart, EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmStream, OutEvent,
    SessionId, StopReason, Usage,
};
use futures::stream;
use futures::StreamExt;

mod common;
use common::collect_until_done;

fn thinking_block() -> serde_json::Value {
    serde_json::json!({ "type": "thinking", "thinking": "let me think", "signature": "sig-1" })
}

/// Streams reasoning deltas (display) *and* the assembled reasoning block
/// (persistence), then text and a clean `Finish` — the shape the Anthropic
/// client produces for a thinking turn.
struct ThinkingLlm;

#[async_trait]
impl Llm for ThinkingLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let events = vec![
            Ok(LlmEvent::Reasoning("let me ".into())),
            Ok(LlmEvent::Reasoning("think".into())),
            Ok(LlmEvent::ContentBlock(ContentPart::reasoning(
                "anthropic",
                "let me think",
                thinking_block(),
            ))),
            Ok(LlmEvent::Text("the answer".into())),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

fn config() -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(|| Box::new(ThinkingLlm) as Box<dyn Llm>),
        default_model: Some("test-model".to_string()),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn thinking_block_emits_a_persisted_reasoning_block() {
    let holly = Holly::spawn(config());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "think about it"))
        .await
        .unwrap();
    let events = collect_until_done(sub, &sid).await;

    let part = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ReasoningBlock { part, .. } => Some(part.clone()),
            _ => None,
        })
        .expect("a ReasoningBlock event");
    // Captured verbatim, signature included — anything less is unreplayable.
    assert_eq!(
        part,
        ContentPart::reasoning("anthropic", "let me think", thinking_block())
    );
}

#[tokio::test]
async fn reasoning_block_rides_its_own_variant_not_search_result() {
    // The two persisted content-block rails must stay distinct: a reader that
    // treats every persisted block as a search result would mislabel reasoning,
    // and pre-existing logs must keep replaying unchanged.
    let holly = Holly::spawn(config());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "think about it"))
        .await
        .unwrap();
    let events = collect_until_done(sub, &sid).await;

    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, OutEvent::ReasoningBlock { .. }))
            .count(),
        1,
        "exactly one reasoning block, not duplicated"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::SearchResult { .. })),
        "a thinking block must not be persisted as a search result"
    );
}

#[tokio::test]
async fn display_deltas_and_the_persisted_block_are_both_emitted() {
    // Capture is additive: the live `ReasoningDelta` rendering channel is
    // untouched by the new persistence rail, and the round's text still streams
    // normally alongside both.
    let holly = Holly::spawn(config());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "think about it"))
        .await
        .unwrap();
    let events = collect_until_done(sub, &sid).await;

    let reasoning_text: String = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ReasoningDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning_text, "let me think");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "the answer")),
        "the round's answer text must still stream"
    );
}
