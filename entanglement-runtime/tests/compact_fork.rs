//! Integration test for compaction's copy-on-write fork (ADR-0101).
//!
//! `/compact` (`InMsg::Oneshot`) never mutates the source session — it emits
//! `OutEvent::Compacted` carrying the summary, and the head forks the summary
//! into a new session via `InMsg::Spawn`. This test drives a real `Holly`
//! through the full path: prompt → compact → capture the `Compacted` event →
//! fork via `Spawn` → assert the new session has the summary as its first
//! message and the source is untouched.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmResponse,
    LlmStream, OutEvent, SessionId, StopReason, Usage,
};
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

/// Collect events for `sid` through `Done` plus the trailing lifecycle `Status`.
async fn collect_until_done(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let mut seen_done = false;
    loop {
        let per_event_deadline = tokio::time::Instant::now()
            + if seen_done {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(3)
            };
        let Ok(recv) = tokio::time::timeout_at(per_event_deadline, sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == Some(sid) => {
                let is_done = matches!(ev, OutEvent::Done { .. });
                out.push(ev);
                if is_done {
                    seen_done = true;
                } else if seen_done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Collect events for `sid`, accepting only those after `after_seq` in `seq`,
/// until a `Done` for that session.
async fn collect_fork_events(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    after_seq: u64,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let mut seen_done = false;
    loop {
        let per_event_deadline = tokio::time::Instant::now()
            + if seen_done {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(3)
            };
        let Ok(recv) = tokio::time::timeout_at(per_event_deadline, sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == Some(sid) => {
                let seq_ok = ev.seq().map(|s| s > after_seq).unwrap_or(true);
                if seq_ok {
                    let is_done = matches!(ev, OutEvent::Done { .. });
                    out.push(ev);
                    if is_done {
                        seen_done = true;
                    } else if seen_done {
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

#[tokio::test]
async fn compact_forks_into_a_new_session_and_preserves_the_source() {
    let (cfg, seen) = scripted(vec![
        "turn reply",
        "summary of the conversation so far",
        "fork continuation",
    ]);
    let holly = Holly::spawn(cfg);
    let source = SessionId::new("source");
    let mut sub = holly.subscribe();

    // 1. Run a turn in the source session so it has history to compact.
    holly
        .send(InMsg::prompt(source.clone(), "hello"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &source).await;

    // 2. Compact: the source is summarized, never mutated.
    holly
        .send(InMsg::Oneshot {
            session: source.clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let compact_events = collect_until_done(&mut sub, &source).await;

    let summary = compact_events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("a Compacted event was emitted");
    assert!(summary.contains("summary of the conversation"));

    // 3. Fork: the head mints a new session and spawns it with the summary, as a
    // fresh root that records the source as its predecessor (ADR-0108). The head
    // would also close the source; this engine-level test leaves it live to prove
    // the copy-on-write property in step 5.
    let fork = SessionId::new_uuid();
    holly
        .send(InMsg::Spawn {
            session: fork.clone(),
            parent: None,
            predecessor: Some(source.clone()),
            agent: "build".to_string(),
            prompt: format!("[Conversation summary]\n\n{summary}"),
        })
        .await
        .unwrap();

    // 4. The successor runs its first turn under the summary prompt, as a root
    // that records its predecessor.
    let fork_events = collect_fork_events(&mut sub, &fork, 0).await;
    assert!(
        fork_events.iter().any(
            |e| matches!(e, OutEvent::SessionStarted { parent, predecessor, root, .. }
                if parent.is_none() && *predecessor == Some(source.clone()) && *root)
        ),
        "the successor is a root with the source as predecessor: {fork_events:?}"
    );
    assert!(
        fork_events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { .. })),
        "the forked session completes a turn: {fork_events:?}"
    );

    // 5. Copy-on-write: the source session's next turn sees the full
    // pre-compaction history, not the summary.
    holly
        .send(InMsg::prompt(source.clone(), "again"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &source).await;

    let seen = seen.lock().unwrap();
    // The last request for the source must still carry the original "hello"
    // prompt — proving the source history survived the compaction.
    let source_last = seen
        .iter()
        .rev()
        .find(|req| {
            req.iter()
                .any(|m| m.text() == "hello" || m.text() == "again")
        })
        .expect("a follow-up source request was recorded");
    assert!(
        source_last
            .iter()
            .any(|m| m.text() == "hello" && m.role == entanglement_core::MessageRole::User),
        "source retains its pre-compaction history: {source_last:?}"
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
    let _ = collect_until_done(&mut sub, &sid).await;

    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("truncated"))));
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));

    // No fork is possible (no summary was emitted), and the source is intact —
    // its next turn still sees "hello".
    holly
        .send(InMsg::prompt(sid.clone(), "next"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;
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
        // compaction summary) is truncated.
        let is_summary = req.system.contains("summarization assistant");
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
