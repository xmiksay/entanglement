//! Regression tests for the context-window budget (#178).
//!
//! Before the fix, an over-window turn emitted a warning `Error` and then
//! **sent the full request anyway** against a fixed 180k ceiling — wrong for a
//! 128k model and a wasted paid round-trip. Now core budgets the history against
//! the model's real `context_window`, compacts (prunes old tool outputs), and
//! refuses the turn if it still won't fit — never streaming the LLM.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmStream, OutEvent, SessionId,
};

/// `Llm` that records whether it was ever asked to stream. The refuse path must
/// never reach it.
struct CountingLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Llm for CountingLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(entanglement_core::stream_from_response(
            entanglement_core::LlmResponse {
                text: "unexpected".into(),
                tool_calls: vec![],
            },
        ))
    }
}

/// A prompt that can't fit even after compaction refuses the turn with a
/// "context window exceeded" Error + Done, and the LLM is never streamed.
#[tokio::test]
async fn over_window_prompt_is_refused_without_sending() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(CountingLlm {
                calls: calls_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        // Tiny window → ~85-token budget; a large prompt blows it and no tool
        // output exists to prune, so the turn is refused.
        context_window: Some(100),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        // ~1143 tokens, far over budget
        .send(InMsg::prompt(sid.clone(), "x".repeat(4000)))
        .await
        .unwrap();

    let mut saw_refusal = false;
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        match ev {
            OutEvent::Error {
                session, message, ..
            } if session == sid && message.contains("context window exceeded") => {
                saw_refusal = true;
            }
            OutEvent::Done { session, .. } if session == sid => {
                saw_done = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        saw_refusal,
        "an over-window turn must emit the refusal Error"
    );
    assert!(
        saw_done,
        "the refused turn must still emit Done so heads unblock"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the LLM must NOT be streamed for a refused over-window turn"
    );
}
