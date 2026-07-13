//! Mid-stream LLM error handling (#181): a stream that dies after partial
//! output must not diverge the committed context from what the user saw. The
//! engine either transparently re-requests (when nothing was shown yet) or
//! commits the partial with an `[interrupted]` marker so the next turn's context
//! matches the display, instead of silently dropping the partial.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmSession, LlmStream, MessageRole,
    OutEvent, SessionId, StopReason, Usage,
};
use futures::stream;
use futures::StreamExt;

/// Streams a partial text chunk then fails mid-stream on the first turn; on the
/// second turn it echoes the assistant messages it was handed so a test can
/// observe what the engine committed to context.
struct MidStreamErrLlm {
    calls: AtomicUsize,
}

#[async_trait]
impl Llm for MidStreamErrLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let events = vec![
                Ok(LlmEvent::Text("partial answer".into())),
                Err(anyhow::anyhow!("connection reset mid-stream")),
            ];
            return Ok(stream::iter(events).boxed());
        }
        let echo = req
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant))
            .map(|m| m.text.clone())
            .collect::<Vec<_>>()
            .join("|");
        let events = vec![
            Ok(LlmEvent::Text(format!("assistant-history:{echo}"))),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

/// Fails immediately (no output) on the first turn, then succeeds on the second.
/// Exercises the transparent single re-request: both attempts happen inside one
/// turn, so a single prompt yields a clean `Done` with no error surfaced.
struct FailThenSucceedLlm {
    calls: AtomicUsize,
}

#[async_trait]
impl Llm for FailThenSucceedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let events = vec![Err(anyhow::anyhow!("stream died before first byte"))];
            return Ok(stream::iter(events).boxed());
        }
        let events = vec![
            Ok(LlmEvent::Text("recovered".into())),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

fn config<F, L>(make: F) -> EngineConfig
where
    F: Fn() -> L + Send + Sync + 'static,
    L: Llm + 'static,
{
    EngineConfig {
        llm_factory: Arc::new(move || LlmSession::new(Box::new(make()))),
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

fn texts(events: &[OutEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::TextDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn mid_stream_error_commits_partial_with_interrupted_marker() {
    let holly = Holly::spawn(config(|| MidStreamErrLlm {
        calls: AtomicUsize::new(0),
    }));
    let sid = SessionId::new("s1");

    // First turn: partial shown, then a mid-stream failure.
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    let first = collect(sub, &sid).await;

    // The partial and the marker are both streamed, so display and context match.
    assert_eq!(texts(&first), "partial answer\n\n[interrupted]");
    assert!(
        first.iter().any(|e| matches!(e, OutEvent::Error { .. })),
        "a mid-stream failure surfaces a recoverable error: {first:?}"
    );

    // Second turn: the LLM echoes the assistant history it was handed. The
    // interrupted partial must be present — the model must not continue as if it
    // had said nothing.
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "continue".into(),
        })
        .await
        .unwrap();
    let second = collect(sub, &sid).await;
    let echoed = texts(&second);
    assert!(
        echoed.contains("partial answer\n\n[interrupted]"),
        "the committed context must carry the interrupted partial: {echoed:?}"
    );
}

#[tokio::test]
async fn stream_failure_before_output_is_re_requested_transparently() {
    let holly = Holly::spawn(config(|| FailThenSucceedLlm {
        calls: AtomicUsize::new(0),
    }));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    // A single prompt recovers within one turn: the re-request's output is shown
    // and no error is surfaced to the user.
    assert_eq!(texts(&events), "recovered");
    assert!(
        !events.iter().any(|e| matches!(e, OutEvent::Error { .. })),
        "a pre-output failure recovers silently: {events:?}"
    );
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}
