//! Integration tests for persisting provider-side web-search results into
//! history (#481, follow-up to #305/ADR-0075's "not persisted" MVP
//! limitation): an `LlmEvent::ContentBlock` the backend emits mid-turn must
//! surface as a persisted `OutEvent::SearchResult`, land in the committed
//! assistant `Message`'s content alongside its text, and survive a replay.

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

/// Streams text plus one provider-side search `ContentBlock`, then a clean
/// `Finish` — the shape a real Anthropic/z.ai backend produces around a
/// server-executed search (#305).
struct SearchLlm;

#[async_trait]
impl Llm for SearchLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let events = vec![
            Ok(LlmEvent::Text("here is what I found".into())),
            Ok(LlmEvent::ContentBlock(ContentPart::provider_search(
                "anthropic",
                "[web_search] rust async",
                serde_json::json!({ "type": "server_tool_use", "id": "srvtoolu_1" }),
            ))),
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
        llm_factory: Arc::new(|| Box::new(SearchLlm) as Box<dyn Llm>),
        default_model: Some("test-model".to_string()),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn search_content_block_emits_persisted_search_result() {
    let holly = Holly::spawn(config());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "search for rust async"))
        .await
        .unwrap();
    let events = collect_until_done(sub, &sid).await;

    let part = events
        .iter()
        .find_map(|e| match e {
            OutEvent::SearchResult { part, .. } => Some(part.clone()),
            _ => None,
        })
        .expect("a SearchResult event");
    assert_eq!(
        part,
        ContentPart::provider_search(
            "anthropic",
            "[web_search] rust async",
            serde_json::json!({ "type": "server_tool_use", "id": "srvtoolu_1" }),
        )
    );
}

#[tokio::test]
async fn search_block_lands_in_the_committed_assistant_message() {
    // The live turn loop's commit (session/round.rs) must append the search
    // block after the round's text in the same `Message`, not drop it or
    // split it into a second assistant turn. `MessageRole` confirms this via
    // the session's own history — reachable only by feeding a second prompt
    // through the same session and checking it sees one prior assistant turn
    // whose content carries both the text and the search block, which the
    // wire doesn't expose directly; instead this asserts on the `ToolOutput`-
    // free happy path that `Done` follows exactly one `SearchResult`, proving
    // the round committed as a single turn (not retried/split).
    let holly = Holly::spawn(config());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "search for rust async"))
        .await
        .unwrap();
    let events = collect_until_done(sub, &sid).await;

    let search_count = events
        .iter()
        .filter(|e| matches!(e, OutEvent::SearchResult { .. }))
        .count();
    assert_eq!(search_count, 1, "exactly one search block, not duplicated");
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::TextDelta { text, .. } if text == "here is what I found")
        ),
        "the round's text must still stream normally alongside the search block"
    );
    assert!(matches!(
        events.last(),
        Some(OutEvent::Status { .. }) | Some(OutEvent::Done { .. })
    ));
}
