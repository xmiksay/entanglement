//! Integration tests for `InMsg::Oneshot` and the `"compact"` op (#324,
//! ADR-0082): a single out-of-band LLM call outside the turn loop, replacing
//! the live history with a summary via `Context::apply_compaction`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmResponse,
    LlmStream, Message, OutEvent, SessionId, StopReason, Usage,
};
use futures::stream;
use futures::StreamExt;

/// A scripted reply queue, popped front-to-back per `stream()` call, that also
/// records every request's `messages` — lets a test assert both what the
/// engine sent to the model and what came back.
struct ScriptedLlm {
    replies: Arc<Mutex<VecDeque<(String, Usage)>>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let (text, usage) = self.replies.lock().unwrap().pop_front().unwrap_or_default();
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

fn scripted(replies: Vec<(&str, Usage)>) -> (EngineConfig, Arc<Mutex<Vec<Vec<Message>>>>) {
    let replies: VecDeque<(String, Usage)> = replies
        .into_iter()
        .map(|(t, u)| (t.to_string(), u))
        .collect();
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

/// Collect events for `sid` through `Done` *and* the `Status` that trails it
/// (`turn.rs`/`ops.rs` both emit `Done` then a lifecycle `Status` — waiting one
/// extra beat after `Done` keeps that trailing `Status` from leaking into the
/// next call's collection).
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

fn kinds(events: &[OutEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            OutEvent::Status { .. } => "status",
            OutEvent::Compacted { .. } => "compacted",
            OutEvent::Usage { .. } => "usage",
            OutEvent::Done { .. } => "done",
            OutEvent::Error { .. } => "error",
            _ => "other",
        })
        .collect()
}

#[tokio::test]
async fn compact_happy_path_emits_the_expected_event_order_and_replaces_context() {
    let (cfg, seen) = scripted(vec![
        ("hi there", Usage::default()),
        (
            "summary: user said hello, agent replied",
            Usage {
                input_tokens: Some(50),
                output_tokens: Some(10),
                ..Usage::default()
            },
        ),
        ("ok", Usage::default()),
    ]);
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    let first_turn = collect_until_done(&mut sub, &sid).await;
    assert!(first_turn
        .iter()
        .any(|e| matches!(e, OutEvent::Done { .. })));

    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let compact_events = collect_until_done(&mut sub, &sid).await;

    // Status::Thinking, Compacted, Usage, Done, Status::Done — in that order.
    assert_eq!(
        kinds(&compact_events),
        vec!["status", "compacted", "usage", "done", "status"],
        "unexpected event order: {compact_events:?}"
    );
    let summary = match &compact_events[1] {
        OutEvent::Compacted { summary, kept, .. } => {
            assert_eq!(*kept, 0);
            summary.clone()
        }
        other => panic!("expected Compacted, got {other:?}"),
    };
    assert!(summary.contains("summary: user said hello"));

    // Run a further turn: its request must see exactly the compacted summary
    // (as a user message) plus the new prompt — the pre-compaction history is
    // gone.
    holly
        .send(InMsg::prompt(sid.clone(), "what's next?"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;

    let seen = seen.lock().unwrap();
    let last_request = seen.last().expect("a third request was recorded");
    assert_eq!(
        last_request.len(),
        2,
        "summary + the new prompt: {last_request:?}"
    );
    assert_eq!(last_request[0].role, entanglement_core::MessageRole::User);
    assert!(last_request[0].text().contains("summary: user said hello"));
    assert_eq!(last_request[1].text(), "what's next?");
}

#[tokio::test]
async fn compact_with_empty_history_is_a_recoverable_error() {
    let (cfg, _seen) = scripted(vec![]);
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

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
        .any(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("no conversation history"))));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));
}

#[tokio::test]
async fn unknown_oneshot_op_is_a_recoverable_error() {
    let (cfg, _seen) = scripted(vec![("hi", Usage::default())]);
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
            op: "frobnicate".to_string(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid).await;

    assert!(events.iter().any(
        |e| matches!(e, OutEvent::Error { message, .. } if message.contains("unknown oneshot op"))
    ));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

/// `ScriptedLlm` variant that sleeps before returning, so a mid-turn `Oneshot`
/// reliably lands in the inbox before the turn's stream resolves.
struct SlowScriptedLlm {
    responses: Mutex<VecDeque<LlmResponse>>,
    delay: Duration,
}

#[async_trait]
impl Llm for SlowScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(self.delay).await;
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

#[tokio::test]
async fn oneshot_arriving_during_a_live_turn_is_deferred_until_it_ends() {
    let delay = Duration::from_millis(150);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(SlowScriptedLlm {
                responses: Mutex::new(VecDeque::from(vec![LlmResponse {
                    text: "turn reply".into(),
                    tool_calls: vec![],
                }])),
                delay,
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
    // Land inside the streaming delay window, before the first turn resolves.
    tokio::time::sleep(Duration::from_millis(20)).await;
    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();

    let events = collect_until_done(&mut sub, &sid).await;
    // The live turn's own Done must land before any Compacted — the oneshot
    // was stashed, not run concurrently or ahead of the turn.
    let done_idx = events
        .iter()
        .position(|e| matches!(e, OutEvent::Done { .. }))
        .expect("the live turn completes");
    assert!(
        events[..done_idx]
            .iter()
            .all(|e| !matches!(e, OutEvent::Compacted { .. })),
        "compact must not run before the live turn's Done: {events:?}"
    );

    // The stashed oneshot replays once the turn ends.
    let compact_events = collect_until_done(&mut sub, &sid).await;
    assert!(compact_events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));
}
