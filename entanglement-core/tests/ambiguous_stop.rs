//! Integration tests for ambiguous-stop bounded retry (ADR-0118): a round
//! that ends with no tool calls and an ambiguous `stop_reason` (the stream
//! closed with no confident signal — as a provider like Ollama does when it
//! drops the connection mid-generation) must retry in place, bounded by
//! `EngineConfig::max_ambiguous_stop_retries`, instead of silently ending the
//! turn. A genuinely confident stop (`EndTurn`) must be unaffected, and
//! `max_turns` must still bound the worst case even when every round is
//! ambiguous.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmStream, OutEvent, SessionId,
    StopReason, ToolCall, Usage,
};
use futures::stream;
use futures::StreamExt;

mod common;
use common::{spawn_tool_executor, unknown_tool};

/// Collect every event for `sid` until `Done` (or a timeout), so a test can
/// inspect the full sequence (retries, warnings, the eventual outcome).
async fn collect_until_done(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let Ok(recv) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == Some(sid) => {
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

/// Always streams a text-only reply with an ambiguous `stop_reason: None` —
/// the shape of a stream that closed with no `finish_reason` ever observed
/// (e.g. Ollama dropping the connection mid-generation).
struct AmbiguousLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Llm for AmbiguousLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = vec![
            Ok(LlmEvent::Text("partial".into())),
            Ok(LlmEvent::Finish {
                stop_reason: None,
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

/// Always streams a clean, deliberate `EndTurn` reply. Counts calls so a test
/// can assert no retry round-trip happened.
struct ConfidentLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Llm for ConfidentLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = vec![
            Ok(LlmEvent::Text("all done".into())),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

/// First call: an ambiguous stop with no tool calls (as if truncated before
/// the model could start a tool call). Second call onward: a real tool call.
/// Models "the nudge worked" — the model recovers on retry.
struct AmbiguousThenToolCallLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Llm for AmbiguousThenToolCallLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = if n == 0 {
            vec![
                Ok(LlmEvent::Text("Creating it now, let's get started".into())),
                Ok(LlmEvent::Finish {
                    stop_reason: None,
                    usage: Usage::default(),
                }),
            ]
        } else {
            vec![
                Ok(LlmEvent::ToolCall(ToolCall {
                    id: format!("t{n}"),
                    name: "unknown-tool".into(),
                    input: "{}".into(),
                    provider_meta: None,
                })),
                Ok(LlmEvent::Finish {
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Usage::default(),
                }),
            ]
        };
        Ok(stream::iter(events).boxed())
    }
}

/// First call: an *empty* ambiguous stop (the stream died before any text — the
/// exact motivating Ollama case, Bug A). Second call: a clean `EndTurn` reply.
/// Models the empty-round path where committing `content: []` would break the
/// strict clients' retry request.
struct EmptyAmbiguousThenDoneLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Llm for EmptyAmbiguousThenDoneLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = if n == 0 {
            // No text at all, ambiguous stop.
            vec![Ok(LlmEvent::Finish {
                stop_reason: None,
                usage: Usage::default(),
            })]
        } else {
            vec![
                Ok(LlmEvent::Text("recovered".into())),
                Ok(LlmEvent::Finish {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                }),
            ]
        };
        Ok(stream::iter(events).boxed())
    }
}

/// An empty ambiguous round recovers on retry and ends cleanly: the retry
/// boundary is emitted, the recovered text reaches the caller, and the turn is
/// not wedged by the empty first round (Bug A / ADR-0118).
#[tokio::test]
async fn empty_ambiguous_round_recovers_and_ends_cleanly() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(EmptyAmbiguousThenDoneLlm {
                calls: calls_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "do something"))
        .await
        .unwrap();

    let events = collect_until_done(sub, &sid).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the empty ambiguous round must be retried once, then recover"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, OutEvent::AmbiguousRetry { .. }))
            .count(),
        1,
        "the empty ambiguous round must emit exactly one retry boundary; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "recovered")),
        "the recovered reply must reach the caller; got {events:?}"
    );
    assert!(
        matches!(events.last(), Some(OutEvent::Done { .. })),
        "the turn must end with Done; got {events:?}"
    );
}

/// An ambiguous stop followed by a real tool call succeeds with no user
/// intervention: the retry's nudge gives the model another chance, and the
/// turn parks on the tool call it eventually produces instead of ending on
/// the first, truncated-looking reply.
#[tokio::test]
async fn ambiguous_stop_then_tool_call_succeeds_without_user_intervention() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(AmbiguousThenToolCallLlm {
                calls: calls_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, unknown_tool);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "create some pages"))
        .await
        .unwrap();

    let events = collect_until_done(sub, &sid).await;

    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "the model must be re-queried at least once after the ambiguous first reply"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolCall { tool, .. } if tool == "unknown-tool")),
        "the tool call the model eventually produced must reach the caller; got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::Error { message, .. } if message.contains("ambiguous"))),
        "no ambiguous-retry warning should fire when the model recovers; got {events:?}"
    );
}

/// A persistently ambiguous model still terminates within the configured
/// retry budget, with a distinct warning `Error` — proving the loop is
/// bounded, not silently succeeding and not looping forever.
#[tokio::test]
async fn persistently_ambiguous_model_terminates_within_budget_with_warning() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(AmbiguousLlm {
                calls: calls_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        max_ambiguous_stop_retries: 2,
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "do something"))
        .await
        .unwrap();

    let events = collect_until_done(sub, &sid).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "1 initial call + 2 retries, then give up — the loop must be bounded"
    );
    let warnings: Vec<_> = events
        .iter()
        .filter(
            |e| matches!(e, OutEvent::Error { message, .. } if message.contains("ambiguous") && message.contains("incomplete")),
        )
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "exactly one ambiguous-stop warning should fire once the budget is exhausted; got {events:?}"
    );
    // Each of the 2 retries persists a seq-bearing `AmbiguousRetry` boundary
    // (ADR-0118) so replay/heads reconstruct the nudge + round split.
    let retries = events
        .iter()
        .filter(|e| matches!(e, OutEvent::AmbiguousRetry { .. }))
        .count();
    assert_eq!(
        retries, 2,
        "each in-place retry must emit a persisted AmbiguousRetry boundary; got {events:?}"
    );
    assert!(
        matches!(events.last(), Some(OutEvent::Done { .. })),
        "the turn must still end cleanly (Done) after giving up; got {events:?}"
    );
}

/// `max_ambiguous_stop_retries = 0` is a true opt-out (ADR-0118): it disables
/// the retry *and* stays silent, restoring the pre-ADR-0118 behavior of
/// committing the reply. The first ambiguous stop must end the turn with no
/// retry round-trip and, crucially, **no** warning `Error`.
#[tokio::test]
async fn zero_retry_budget_opts_out_silently() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(AmbiguousLlm {
                calls: calls_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        max_ambiguous_stop_retries: 0,
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "do something"))
        .await
        .unwrap();

    let events = collect_until_done(sub, &sid).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a zero budget must not trigger any retry round-trip"
    );
    assert!(
        !events.iter().any(|e| matches!(e, OutEvent::Error { .. })),
        "opting out with a zero budget must not surface any warning; got {events:?}"
    );
    assert!(
        matches!(events.last(), Some(OutEvent::Done { .. })),
        "the turn must still end with Done; got {events:?}"
    );
}

/// A genuinely clean `EndTurn` stop is unaffected: the turn ends on the first
/// round, with no retry round-trip and no warning.
#[tokio::test]
async fn clean_end_turn_stop_is_unaffected() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ConfidentLlm {
                calls: calls_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .unwrap();

    let events = collect_until_done(sub, &sid).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a confident EndTurn stop must not trigger any retry round-trip"
    );
    assert!(
        !events.iter().any(|e| matches!(e, OutEvent::Error { .. })),
        "a clean stop must not emit any warning; got {events:?}"
    );
    assert!(
        matches!(events.last(), Some(OutEvent::Done { .. })),
        "the turn must end with Done; got {events:?}"
    );
}

/// `max_turns` still composes as the outer backstop even when every round is
/// ambiguous: with a small `max_turns` and a large `max_ambiguous_stop_retries`,
/// the turn ends via the existing "maximum turn limit" Error, not the
/// ambiguous-retry warning — the two counters are independent, and
/// `max_turns` wins first.
#[tokio::test]
async fn max_turns_still_bounds_ambiguous_retries() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = calls.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(AmbiguousLlm {
                calls: calls_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        max_turns: 3,
        max_ambiguous_stop_retries: 100,
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "spin"))
        .await
        .unwrap();

    let mut saw_limit = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if let OutEvent::Error {
            session, message, ..
        } = ev
        {
            if session == sid && message.contains("maximum turn limit") {
                saw_limit = true;
                break;
            }
        }
    }
    assert!(
        saw_limit,
        "max_turns must still trip even when every round is ambiguous, before the \
         (much larger) ambiguous-retry budget would"
    );
    assert!(
        calls.load(Ordering::SeqCst) <= 4,
        "loop must stop at the configured max_turns cap (3), streamed {} times",
        calls.load(Ordering::SeqCst)
    );
}
