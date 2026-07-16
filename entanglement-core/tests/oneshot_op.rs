//! Integration tests for `InMsg::Oneshot` and the `"compact"` op (#324,
//! ADR-0082 → ADR-0101): a single out-of-band LLM call outside the turn loop.
//! **Copy-on-write (ADR-0101):** `compact` never mutates the source session —
//! it emits `OutEvent::Compacted` carrying the summary, which a head forks into
//! a new session. The source `Context` is always left intact.

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

/// A `ScriptedLlm` whose first reply carries a `StopReason::MaxTokens`, so a
/// compaction summary is truncated — the op must reject it and leave the
/// source `Context` untouched.
struct TruncatingLlm {
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Llm for TruncatingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let events = vec![
            Ok(LlmEvent::Text("a cut-off fragment".to_string())),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::MaxTokens),
                usage: Usage::default(),
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

fn truncating() -> (EngineConfig, Arc<Mutex<Vec<Vec<Message>>>>) {
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
async fn compact_happy_path_emits_compacted_and_leaves_source_intact() {
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

    // Copy-on-write (ADR-0101): the source `Context` is untouched — a further
    // turn sees the full pre-compaction history plus the new prompt, not the
    // summary.
    holly
        .send(InMsg::prompt(sid.clone(), "what's next?"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;

    let seen = seen.lock().unwrap();
    let last_request = seen.last().expect("a third request was recorded");
    assert!(
        last_request.len() >= 2,
        "source keeps its full history + the new prompt: {last_request:?}"
    );
    assert_eq!(last_request[0].role, entanglement_core::MessageRole::User);
    assert_eq!(last_request[0].text(), "hello");
    let tail = last_request.last().expect("a final user message");
    assert_eq!(tail.text(), "what's next?");
}

#[tokio::test]
async fn compact_with_kept_preserves_the_tail_verbatim_in_the_summary() {
    let (cfg, _seen) = scripted(vec![
        ("hi there", Usage::default()),
        ("ok2", Usage::default()),
        ("summary: user said hello, agent replied", Usage::default()),
    ]);
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;
    holly
        .send(InMsg::prompt(sid.clone(), "second"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;

    // History: [user hello, assistant "hi there", user second, assistant
    // "ok2"] — kept=2 lands exactly on the "second" user message, a safe
    // boundary, so no clamping happens.
    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::json!({ "kept": 2 }),
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid).await;

    let (summary, kept) = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted { summary, kept, .. } => Some((summary.clone(), *kept)),
            _ => None,
        })
        .expect("a Compacted event was emitted");
    assert_eq!(kept, 2, "the requested boundary was already safe");
    assert!(
        summary.contains("summary: user said hello"),
        "the LLM summary of the head is present: {summary}"
    );
    assert!(
        summary.contains("second") && summary.contains("ok2"),
        "the tail rides verbatim, not resummarized: {summary}"
    );
}

#[tokio::test]
async fn compact_with_kept_clamps_to_a_safe_boundary() {
    let (cfg, _seen) = scripted(vec![
        ("hi there", Usage::default()),
        ("summary of the first turn", Usage::default()),
    ]);
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;

    // History: [user hello, assistant "hi there"] (len 2). Requesting kept=1
    // would naively split at index 1 (the `Assistant` reply) — not a `User`
    // boundary — so it clamps forward; no later `User` message exists, so it
    // collapses to 0.
    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::json!({ "kept": 1 }),
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid).await;

    let kept = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted { kept, .. } => Some(*kept),
            _ => None,
        })
        .expect("a Compacted event was emitted");
    assert_eq!(kept, 0, "clamped to the nearest safe (empty) tail");
}

#[tokio::test]
async fn compact_with_kept_covering_everything_is_a_recoverable_error() {
    let (cfg, _seen) = scripted(vec![("hi there", Usage::default())]);
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
            args: serde_json::json!({ "kept": 999 }),
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid).await;

    assert!(events.iter().any(
        |e| matches!(e, OutEvent::Error { message, .. } if message.contains("nothing left to summarize"))
    ));
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));
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
async fn compact_with_truncated_summary_is_rejected_and_source_is_unchanged() {
    let (cfg, seen) = truncating();
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

    // A truncated summary (StopReason::MaxTokens) is rejected — Error + Done,
    // no Compacted.
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("truncated"))));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));

    // Copy-on-write + early refusal: a follow-up turn must see the full
    // pre-compaction history — the truncated fragment never landed anywhere.
    holly
        .send(InMsg::prompt(sid.clone(), "again"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;
    let seen = seen.lock().unwrap();
    let last = seen.last().expect("a follow-up request was recorded");
    let first = last
        .first()
        .expect("the original user message is still present");
    assert_eq!(first.text(), "hello");
}

#[tokio::test]
async fn compact_with_oversized_transcript_is_rejected_before_the_llm_is_called() {
    let (cfg, seen) = scripted(vec![
        ("hi", Usage::default()),
        ("summary", Usage::default()),
    ]);
    // Feed a transcript so large that compaction's input guard fires. (The first
    // turn with this message refuses for the same reason — that's fine; the
    // point is the compaction op's own guard catches the oversize *before*
    // shipping a summarization request the provider would 4xx.)
    let huge = "x".repeat(180_000 * 4 + 1_000); // ~4x chars/token → well over 180k tokens
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), huge)).await.unwrap();
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

    assert!(events.iter().any(
        |e| matches!(e, OutEvent::Error { message, .. } if message.contains("exceeds") && message.contains("context budget"))
    ));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));

    // The summarization LLM must never have been called — the op refused
    // before shipping the request. (The refused first turn didn't call it
    // either, so `seen` stays empty.)
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().all(|req| req
            .iter()
            .all(|m| !m.text().contains("Summarize the conversation transcript"))),
        "no summarization request reached the LLM: {seen:?}"
    );
}

#[tokio::test]
async fn compact_with_an_oversized_kept_tail_is_rejected_before_the_llm_is_called() {
    let (cfg, seen) = scripted(vec![("hi there", Usage::default())]);
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;

    // The second prompt itself is too large to fit the context budget alone —
    // its own turn refuses (a lone oversized message can't be pruned), but the
    // prompt still lands in `ctx.messages()`. `kept=1` naively (and safely,
    // since it's the tail's own `User` message) selects just that message —
    // small head, oversized tail.
    let huge = "x".repeat(180_000 * 4 + 1_000);
    holly.send(InMsg::prompt(sid.clone(), huge)).await.unwrap();
    let _ = collect_until_done(&mut sub, &sid).await;

    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".to_string(),
            args: serde_json::json!({ "kept": 1 }),
        })
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid).await;

    assert!(events.iter().any(
        |e| matches!(e, OutEvent::Error { message, .. } if message.contains("kept trailing messages") && message.contains("exceed"))
    ));
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));

    // The summarization LLM must never have been called — the tail guard
    // fires before the head-summarization request ships.
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().all(|req| req
            .iter()
            .all(|m| !m.text().contains("Summarize the conversation transcript"))),
        "no summarization request reached the LLM: {seen:?}"
    );
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
