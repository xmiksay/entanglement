//! Integration tests for auto-summarize on context overflow (#398, ADR-0103).
//!
//! Since ADR-0205 the automatic path forks like every other compaction: the
//! overflowing session is summarized into a **successor** and retired, and the
//! turn goes on there. These tests drive a real `Holly` through an overflowing
//! turn and assert: the `Compacted { auto: true, .. }` event fires, a successor
//! picks the turn up under the summarized context instead of the turn being
//! refused, and `EngineConfig::auto_compact = false` still falls through to the
//! prune/refuse path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmStream, Message, OutEvent, SessionId,
    StopReason, Usage,
};
use futures::stream;
use futures::StreamExt;

/// Replies "ok" to any ordinary turn request; a compaction request instead
/// replies with a scripted summary. Both shapes end on a "Summarize the
/// conversation …" instruction — the session-backend one after the replayed
/// history, the rendered one around a transcript (ADR-0202). Records
/// every request's messages so a test can assert what shipped post-compaction.
struct ScriptedLlm {
    summary: String,
    turn_calls: Arc<AtomicUsize>,
    summary_calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let is_summary = req
            .messages
            .last()
            .is_some_and(|m| m.text().contains("Summarize the conversation"));
        let text = if is_summary {
            self.summary_calls.fetch_add(1, Ordering::SeqCst);
            self.summary.clone()
        } else {
            self.turn_calls.fetch_add(1, Ordering::SeqCst);
            "ok".to_string()
        };
        let events = vec![
            Ok(LlmEvent::Text(text)),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

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
                // A compacted session is retired without a `Done` (ADR-0205),
                // so `SessionEnded` is terminal too.
                if matches!(ev, OutEvent::SessionEnded { .. }) {
                    out.push(ev);
                    break;
                }
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

/// Three short turns (each with a distinct marker so the test can tell which
/// one survives compaction) then one large prompt overflows a small
/// `context_window` — chosen with a wide margin so the summarize guard's own
/// checks (transcript and kept-tail size against the same budget) comfortably
/// pass too. `Context::safe_kept` clamps `AUTO_COMPACT_KEEP_TAIL` (4) forward
/// to the next `User` message, landing the tail on turn 3's prompt onward —
/// turns 1 and 2 land in the summarized head.
async fn run_three_turns_then_overflow(holly: &Holly, sid: &SessionId) -> Vec<OutEvent> {
    let mut sub = holly.subscribe();
    for i in 0..3 {
        holly
            .send(InMsg::prompt(
                sid.clone(),
                format!("turn-{i}-marker: {}", "y".repeat(490)),
            ))
            .await
            .unwrap();
        let _ = collect_until_done(&mut sub, sid).await;
    }
    holly
        .send(InMsg::prompt(sid.clone(), "x".repeat(11_000)))
        .await
        .unwrap();
    collect_until_done(&mut sub, sid).await
}

/// Drain until `source` announces its compaction successor, then keep draining
/// until that successor's turn finishes. Returns the source's events, the
/// successor's id, and the successor's events.
async fn until_successor_done(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    source: &SessionId,
) -> (Vec<OutEvent>, SessionId, Vec<OutEvent>) {
    let mut source_events = Vec::new();
    let mut successor: Option<SessionId> = None;
    let mut successor_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if let OutEvent::SessionStarted {
            session: succ,
            predecessor: Some(p),
            ..
        } = &ev
        {
            if p == source {
                successor = Some(succ.clone());
            }
        }
        match &successor {
            Some(succ) if ev.session() == Some(succ) => {
                let done = matches!(ev, OutEvent::Done { .. });
                successor_events.push(ev);
                if done {
                    break;
                }
            }
            _ if ev.session() == Some(source) => source_events.push(ev),
            _ => {}
        }
    }
    let successor = successor.expect("the compaction announced a successor");
    (source_events, successor, successor_events)
}

#[tokio::test]
async fn overflow_forks_a_successor_and_the_turn_proceeds_there() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let seen2 = seen.clone();
    let turn_calls2 = turn_calls.clone();
    let summary_calls2 = summary_calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                summary: "auto-summary: three short exchanges happened".to_string(),
                turn_calls: turn_calls2.clone(),
                summary_calls: summary_calls2.clone(),
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        context_window: Some(4_000), // limit = 3400 tokens
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");

    let mut sub = holly.subscribe();
    for i in 0..3 {
        holly
            .send(InMsg::prompt(
                sid.clone(),
                format!("turn-{i}-marker: {}", "y".repeat(490)),
            ))
            .await
            .unwrap();
        let _ = collect_until_done(&mut sub, &sid).await;
    }
    holly
        .send(InMsg::prompt(sid.clone(), "x".repeat(11_000)))
        .await
        .unwrap();
    let (events, successor, successor_events) = until_successor_done(&mut sub, &sid).await;

    let compacted = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted {
                summary,
                kept,
                auto,
                ..
            } => Some((summary.clone(), *kept, *auto)),
            _ => None,
        })
        .expect("auto-compact emitted a Compacted event");
    assert!(compacted.2, "the event is marked auto: true");
    assert!(compacted.0.contains("auto-summary"));
    assert_eq!(
        compacted.1, 3,
        "safe_kept clamps kept=4 forward to the next User boundary (turn 3 onward)"
    );

    // The source is retired at the fork; the turn completes in the successor.
    assert_ne!(successor, sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::SessionEnded { .. })),
        "the compacted source is retired: {events:?}"
    );
    assert!(
        successor_events
            .iter()
            .any(|e| matches!(e, OutEvent::Done { .. })),
        "the turn proceeds in the successor instead of refusing: {successor_events:?}"
    );
    assert!(
        !events
            .iter()
            .chain(successor_events.iter())
            .any(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("context window exceeded"))),
        "no refusal error: {events:?} {successor_events:?}"
    );

    assert_eq!(
        summary_calls.load(Ordering::SeqCst),
        1,
        "exactly one summarization round-trip"
    );
    assert_eq!(
        turn_calls.load(Ordering::SeqCst),
        4,
        "3 prior turns + the successor's own first request"
    );

    // The request the successor actually sent carries the summarized head
    // (turns 0 and 1 gone, folded into the summary) plus turn 2's exchange
    // verbatim (the safe kept-tail boundary) — not the raw 4-turn history.
    let seen = seen.lock().unwrap();
    let last_request = seen.last().expect("the successor's request");
    let joined: String = last_request
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("turn-0-marker"),
        "turn 0 was folded into the summary, not sent verbatim: {joined}"
    );
    assert!(
        !joined.contains("turn-1-marker"),
        "turn 1 was folded into the summary, not sent verbatim: {joined}"
    );
    assert!(
        joined.contains("turn-2-marker"),
        "turn 2 rides verbatim as the kept tail: {joined}"
    );
    assert!(
        joined.contains("auto-summary"),
        "the summarized head is present as the new leading message: {joined}"
    );
    assert!(
        joined.contains("continues from a compaction"),
        "the successor's seed says what it continues from: {joined}"
    );
}

#[tokio::test]
async fn auto_compact_disabled_falls_back_to_the_old_refuse_behavior() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            struct CountingLlm(Arc<AtomicUsize>);
            #[async_trait]
            impl Llm for CountingLlm {
                async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(entanglement_core::stream_from_response(
                        entanglement_core::LlmResponse {
                            text: "unexpected".into(),
                            tool_calls: vec![],
                        },
                    ))
                }
            }
            Box::new(CountingLlm(calls_for_factory.clone())) as Box<dyn Llm>
        }),
        context_window: Some(100), // limit = 85 tokens — no tool output to prune
        auto_compact: false,
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "x".repeat(4_000)))
        .await
        .unwrap();
    let events = collect_until_done(&mut sub, &sid).await;

    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::Error { message, .. } if message.contains("context window exceeded"))
        ),
        "auto_compact: false must preserve the pre-#398 refusal: {events:?}"
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, OutEvent::Compacted { .. })));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the LLM must never be streamed for a refused over-window turn"
    );
}

/// The `summarize` aux-model pin (Issue 5) routes compaction to its own
/// backend: the auto-summarize call must land on the resolver's LLM, while the
/// ordinary turn calls keep using the session's own.
#[tokio::test]
async fn auto_compact_uses_the_summarize_aux_model_when_pinned() {
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    // Counts calls that landed on the *aux* backend specifically.
    let aux_calls = Arc::new(AtomicUsize::new(0));

    let (turn_calls2, summary_calls2, seen2) =
        (turn_calls.clone(), summary_calls.clone(), seen.clone());
    let aux_for_resolver = aux_calls.clone();

    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                summary: "session-model summary (should NOT be used)".to_string(),
                turn_calls: turn_calls2.clone(),
                summary_calls: summary_calls2.clone(),
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        context_window: Some(4_000),
        aux_llm_resolver: Some(Arc::new(move |purpose: &str| {
            // Core must ask for exactly this purpose key.
            assert_eq!(purpose, "summarize");
            let aux_calls = aux_for_resolver.clone();
            Some(entanglement_core::ResolvedModel {
                provider: "aux-provider".to_string(),
                model: "aux-model".to_string(),
                llm_factory: Arc::new(move || {
                    Box::new(ScriptedLlm {
                        summary: "aux-summary: compacted by the pinned model".to_string(),
                        turn_calls: aux_calls.clone(),
                        summary_calls: aux_calls.clone(),
                        seen: Arc::new(Mutex::new(Vec::new())),
                    }) as Box<dyn Llm>
                }),
                generation: None,
                context_window: None,
            })
        })),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");

    let events = run_three_turns_then_overflow(&holly, &sid).await;

    let summary = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted { summary, auto, .. } if *auto => Some(summary.clone()),
            _ => None,
        })
        .expect("expected an auto Compacted event");

    // The summary came from the pinned aux backend, not the session's own.
    assert!(
        summary.contains("aux-summary"),
        "compaction must run on the pinned aux model, got: {summary}"
    );
    assert_eq!(
        summary_calls.load(Ordering::SeqCst),
        0,
        "the session's own backend must not have been asked to summarize"
    );
    assert_eq!(
        aux_calls.load(Ordering::SeqCst),
        1,
        "exactly one summarization call should land on the aux backend"
    );
    // Ordinary turns still run on the session's own backend.
    assert!(turn_calls.load(Ordering::SeqCst) > 0);
}

/// A resolver that returns `None` (no pin, or a pin the catalog forgot) falls
/// back to the session's own backend — byte-identical to pre-Issue-5.
#[tokio::test]
async fn auto_compact_falls_back_to_the_session_model_when_unpinned() {
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (turn_calls2, summary_calls2, seen2) =
        (turn_calls.clone(), summary_calls.clone(), seen.clone());

    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                summary: "session-summary: used as the fallback".to_string(),
                turn_calls: turn_calls2.clone(),
                summary_calls: summary_calls2.clone(),
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        context_window: Some(4_000),
        aux_llm_resolver: Some(Arc::new(|_purpose: &str| None)),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");

    let events = run_three_turns_then_overflow(&holly, &sid).await;

    let summary = events
        .iter()
        .find_map(|e| match e {
            OutEvent::Compacted { summary, auto, .. } if *auto => Some(summary.clone()),
            _ => None,
        })
        .expect("expected an auto Compacted event");
    assert!(summary.contains("session-summary"));
    assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
}
