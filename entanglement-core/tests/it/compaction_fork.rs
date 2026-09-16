//! The compaction fork's lifecycle (ADR-0205): the prune-only fallback
//! announcing itself, and the hand-off of a tool result that was still in
//! flight when its session compacted away.
//!
//! The summary path is covered by `auto_compact`/`oneshot_op`; what is specific
//! here is the fallback nobody used to hear about (ADR-0121's silence) and the
//! edge where a frame names a session that no longer exists.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{
    stream_from_response, CompactionMode, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, SessionId, ToolCall, ToolSpec,
};

use crate::common::collect_until_done;

/// Calls `probe` on its first request, then answers with text — enough to get
/// one bulky tool result into the history, which is what the prune reclaims.
struct ToolThenText {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Llm for ToolThenText {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(stream_from_response(if n == 0 {
            LlmResponse {
                text: String::new(),
                tool_calls: vec![ToolCall::new("c1", "probe", "{}")],
            }
        } else {
            LlmResponse {
                text: "carrying on".into(),
                tool_calls: vec![],
            }
        }))
    }
}

/// A session whose window is small enough that one bulky tool result overflows
/// it, with auto-summarize off so recovery takes the prune fallback.
fn prune_only_engine() -> Holly {
    let calls = Arc::new(AtomicUsize::new(0));
    Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ToolThenText {
                calls: calls.clone(),
            }) as Box<dyn Llm>
        }),
        tool_specs: vec![ToolSpec::new("probe", "a test tool")],
        context_window: Some(4_000), // ~3400-token input budget
        auto_compact: false,
        ..EngineConfig::default()
    })
}

/// Drive one overflowing tool round and return the source's events, the
/// successor's id, and the still-live subscription — handed back so a caller
/// can keep watching the successor without racing its first turn.
async fn prune_fork(
    holly: &Holly,
    sid: &SessionId,
) -> (
    Vec<OutEvent>,
    SessionId,
    tokio::sync::broadcast::Receiver<OutEvent>,
) {
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    // Answer the tool call with an output far over the budget.
    let request_id = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(OutEvent::ToolExec {
                session,
                request_id,
                ..
            }) = sub.recv().await
            {
                if session == *sid {
                    return request_id;
                }
            }
        }
    })
    .await
    .expect("the turn parked on a tool call");
    holly
        .send(InMsg::tool_result(
            sid.clone(),
            request_id,
            "x".repeat(14_000),
        ))
        .await
        .unwrap();

    let mut source_events = Vec::new();
    let successor = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted {
                    session,
                    predecessor: Some(p),
                    ..
                }) if p == *sid => return session,
                Ok(ev) if ev.session() == Some(sid) => source_events.push(ev),
                Ok(_) => {}
                Err(_) => panic!("event stream closed before the fork"),
            }
        }
    })
    .await
    .expect("the prune fallback forked a successor");
    (source_events, successor, sub)
}

/// ADR-0121's silence is retired: the prune-only fallback emits `Compacted`
/// like any other compaction, because a head has to follow the fork.
#[tokio::test]
async fn the_prune_fallback_announces_itself_and_forks() {
    let holly = prune_only_engine();
    let sid = SessionId::new("s1");
    let (events, successor, _sub) = prune_fork(&holly, &sid).await;

    let (summary, auto, mode) = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted {
                summary,
                auto,
                mode,
                ..
            } => Some((summary.clone(), *auto, *mode)),
            _ => None,
        })
        .expect("the prune fallback emitted a Compacted event");
    assert!(auto, "an overflow recovery is an automatic compaction");
    assert_eq!(
        mode,
        CompactionMode::Prune,
        "a head must be able to tell a prune from a summary"
    );
    assert!(
        summary.contains("[tool output pruned to fit the context window]"),
        "the pruned transcript seeds the successor: {summary}"
    );
    assert_ne!(successor, sid, "the successor is a fresh session");
}

/// A tool result that was already in flight when its session compacted away
/// must not come back as a "session is closed" error: the supervisor hands it
/// to the successor that inherited the turn (ADR-0205).
#[tokio::test]
async fn a_tool_result_for_a_retired_source_is_handed_to_its_successor() {
    let holly = prune_only_engine();
    let sid = SessionId::new("s1");
    let (_events, successor, sub) = prune_fork(&holly, &sid).await;

    // Sent the instant the fork is observed, while the successor's first turn
    // is still in flight — exactly when a duplicate or slow executor reply for
    // the pre-fork batch would land, still addressed to the retired source.
    holly
        .send(InMsg::tool_result(sid.clone(), "c1", "a late reply"))
        .await
        .unwrap();

    // Same subscription the fork was observed on, so the successor's `Done`
    // cannot slip past between the two.
    let successor_events = collect_until_done(sub, &successor).await;
    assert!(
        successor_events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { .. })),
        "the successor's turn still finishes: {successor_events:?}"
    );

    let refusals: Vec<&OutEvent> = successor_events
        .iter()
        .filter(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("closed")))
        .collect();
    assert!(
        refusals.is_empty(),
        "a late result must be redirected to the successor, not refused as a \
         closed id: {refusals:?}"
    );
}
