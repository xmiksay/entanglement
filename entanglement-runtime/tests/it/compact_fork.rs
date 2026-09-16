//! Compaction's successor fork, end to end through the runtime's persistence
//! (#324, ADR-0101/0110, generalized by ADR-0205).
//!
//! The head no longer forks anything: `/compact` (`InMsg::Oneshot`) makes the
//! *engine* mint a successor session, seed it with the summary and retire the
//! source. What this test pins down is the part core cannot: that the pair
//! lands on disk as two resumable root sessions, with the successor's seed
//! recorded as its first prompt (ADR-0113's `Spawn`-prompt synthesis) so the
//! `sessions` listing shows a readable lineage rather than an empty successor.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmResponse,
    LlmStream, OutEvent, SessionId, StopReason, Usage,
};
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::session_store;
use futures::stream;
use futures::StreamExt;

/// A scripted reply queue that also records each request's messages, so the
/// test can assert what the engine sent to the model.
struct ScriptedLlm {
    replies: Arc<Mutex<Vec<(String, Usage)>>>,
    seen: Arc<Mutex<Vec<Vec<entanglement_core::Message>>>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let (text, usage) = self
            .replies
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(("ok".to_string(), Usage::default()));
        let events = vec![
            Ok(LlmEvent::Text(text)),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage,
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

fn scripted(
    replies: Vec<&str>,
) -> (
    EngineConfig,
    Arc<Mutex<Vec<Vec<entanglement_core::Message>>>>,
) {
    let mut replies: Vec<(String, Usage)> = replies
        .into_iter()
        .map(|t| (t.to_string(), Usage::default()))
        .collect();
    replies.reverse(); // pop() takes from the back → reverse for FIFO order
    let replies = Arc::new(Mutex::new(replies));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                replies: replies.clone(),
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    (cfg, seen)
}

/// Collect `sid`'s events until it finishes a turn or is retired.
async fn collect_until_settled(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() != Some(sid) {
            continue;
        }
        let terminal = matches!(ev, OutEvent::Done { .. } | OutEvent::SessionEnded { .. });
        out.push(ev);
        if terminal {
            break;
        }
    }
    out
}

/// Wait for the successor `source` compacted into.
async fn await_successor(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    source: &SessionId,
) -> SessionId {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(OutEvent::SessionStarted {
                session,
                predecessor: Some(p),
                ..
            }) = sub.recv().await
            {
                if p == *source {
                    return session;
                }
            }
        }
    })
    .await
    .expect("the compaction announced a successor")
}

#[tokio::test]
async fn compact_forks_a_successor_and_both_sessions_are_listed() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let cwd = tmp.path().to_path_buf();
    let (cfg, seen) = scripted(vec![
        "turn reply",
        "summary of the conversation so far",
        "fork continuation",
    ]);
    let holly = Holly::spawn(cfg);
    let _tap = spawn_persistence_subscriber(&holly, cwd.clone());
    let source = SessionId::new("source");
    let mut sub = holly.subscribe();

    // 1. A turn, so the session has history worth compacting.
    holly
        .send(InMsg::prompt(source.clone(), "hello"))
        .await
        .unwrap();
    let _ = collect_until_settled(&mut sub, &source).await;

    // 2. Compact. The engine summarizes, forks, and retires the source — the
    //    head sends no `Spawn` and no `CloseSession` of its own.
    holly
        .send(InMsg::Oneshot {
            session: source.clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let compact_events = collect_until_settled(&mut sub, &source).await;
    let summary = compact_events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("a Compacted event was emitted");
    assert!(summary.contains("summary of the conversation"));

    // 3. The successor is a root recording its predecessor, and it runs.
    let successor = await_successor(&mut sub, &source).await;
    assert_ne!(successor, source);
    let successor_events = collect_until_settled(&mut sub, &successor).await;
    assert!(
        successor_events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { .. })),
        "the successor completes its first turn: {successor_events:?}"
    );

    // 4. It started from the summary, not an empty history.
    {
        let seen = seen.lock().unwrap();
        let seeded = seen.last().expect("the successor's request");
        assert!(
            seeded[0].text().contains("summary of the conversation"),
            "the successor is seeded with the summary: {seeded:?}"
        );
    }

    // 5. Both land on disk as separate resumable roots, and the successor's
    //    seed is recorded as its first prompt — so a `sessions` listing reads
    //    as a lineage instead of showing a blank successor.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sessions = session_store::list_sessions(&cwd).expect("listing the session store");
    let ids: Vec<&SessionId> = sessions.iter().map(|m| &m.id).collect();
    assert!(
        ids.contains(&&source) && ids.contains(&&successor),
        "predecessor and successor are both listed: {ids:?}"
    );
    let successor_meta = sessions
        .iter()
        .find(|m| m.id == successor)
        .expect("the successor is listed");
    assert!(successor_meta.root, "the successor is a root, not a child");
    assert!(
        successor_meta
            .first_prompt
            .as_deref()
            .unwrap_or_default()
            .contains("Conversation summary"),
        "the successor's seed is its recorded first prompt: {successor_meta:?}"
    );
}

#[tokio::test]
async fn compact_with_truncated_summary_is_rejected_no_fork() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(TruncatingLlm {
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    let _ = collect_until_settled(&mut sub, &sid).await;

    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let events = collect_until_settled(&mut sub, &sid).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("truncated"))));
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));

    // No fork happened, so the source is still live and still holds "hello".
    holly
        .send(InMsg::prompt(sid.clone(), "next"))
        .await
        .unwrap();
    let _ = collect_until_settled(&mut sub, &sid).await;
    let seen = seen.lock().unwrap();
    let last = seen.last().expect("a follow-up request was recorded");
    assert!(last
        .iter()
        .any(|m| m.text() == "hello" && m.role == entanglement_core::MessageRole::User));
}

/// An LLM whose summary reply is truncated (`StopReason::MaxTokens`).
struct TruncatingLlm {
    seen: Arc<Mutex<Vec<Vec<entanglement_core::Message>>>>,
}

#[async_trait]
impl Llm for TruncatingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // First call (the live turn) gets a clean reply; the second (the
        // compaction summary) is truncated. Both summary shapes (ADR-0202)
        // end on a "Summarize the conversation …" instruction.
        let is_summary = req
            .messages
            .last()
            .is_some_and(|m| m.text().contains("Summarize the conversation"));
        self.seen.lock().unwrap().push(req.messages.to_vec());
        if is_summary {
            let events = vec![
                Ok(LlmEvent::Text("a cut-off fragment".to_string())),
                Ok(LlmEvent::Finish {
                    stop_reason: Some(StopReason::MaxTokens),
                    usage: Usage::default(),
                }),
            ];
            Ok(stream::iter(events).boxed())
        } else {
            let resp = LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            };
            Ok(stream_from_response(resp))
        }
    }
}
