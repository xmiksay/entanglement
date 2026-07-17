//! Integration tests for auto-summarize on context overflow (#398, ADR-0103).
//!
//! Unlike manual `/compact` (copy-on-write, ADR-0101), `session/turn.rs`'s
//! automatic path mutates the live session's `Context` **in place** before
//! continuing the turn — a turn mid-flight has no head to fork into. These
//! tests drive a real `Holly` through an overflowing turn and assert: the
//! `Compacted { auto: true, .. }` event fires, the turn proceeds under the
//! summarized context instead of refusing, and `EngineConfig::auto_compact =
//! false` restores the old prune-only (or refuse) behavior.

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

/// Replies "ok" to any ordinary turn request; a request whose system prompt
/// marks it as the summarizer instead replies with a scripted summary. Records
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
        let is_summary = req.system.contains("summarization assistant");
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

#[tokio::test]
async fn overflow_triggers_auto_compact_and_the_turn_proceeds() {
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

    let events = run_three_turns_then_overflow(&holly, &sid).await;

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

    // The overflowing turn still completes — Done, not a refusal Error.
    assert!(
        events.iter().any(|e| matches!(e, OutEvent::Done { .. })),
        "the turn proceeds after auto-compact instead of refusing: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("context window exceeded"))),
        "no refusal error: {events:?}"
    );

    assert_eq!(
        summary_calls.load(Ordering::SeqCst),
        1,
        "exactly one summarization round-trip"
    );
    assert_eq!(
        turn_calls.load(Ordering::SeqCst),
        4,
        "3 prior turns + the overflowing turn's own (post-compaction) request"
    );

    // The request the overflowing turn actually sent carries the summarized
    // head (turns 0 and 1 gone, folded into the summary) plus turn 2's
    // exchange verbatim (the safe kept-tail boundary) and the overflowing
    // prompt itself — not the raw, un-compacted 4-turn history.
    let seen = seen.lock().unwrap();
    let last_request = seen.last().expect("the overflowing turn's request");
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
