//! Integration test for streamed tool-call argument fragments (#194): a
//! provider that emits `LlmEvent::ToolCallDelta` before the assembled
//! `LlmEvent::ToolCall` must surface each fragment as an
//! `OutEvent::ToolCallDelta` — correlated to the eventual `ToolExec`/`ToolCall`
//! by `request_id` — while still driving the tool round-trip normally.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmStream, OutEvent, SessionId,
    StopReason, ToolCall, Usage,
};
use futures::stream::{self, StreamExt};

mod common;
use common::spawn_tool_executor;

/// Scripted backend that replays a queued list of raw event batches, one per
/// `stream()` call. Lets a test drive the exact `LlmEvent` sequence — including
/// `ToolCallDelta` — that a real client would produce.
struct EventScriptLlm {
    rounds: Mutex<Vec<Vec<LlmEvent>>>,
}

impl EventScriptLlm {
    fn new(mut rounds: Vec<Vec<LlmEvent>>) -> Self {
        rounds.reverse();
        Self {
            rounds: Mutex::new(rounds),
        }
    }
}

#[async_trait]
impl Llm for EventScriptLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let events = self.rounds.lock().unwrap().pop().unwrap_or_else(|| {
            vec![LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }]
        });
        Ok(stream::iter(events.into_iter().map(Ok)).boxed())
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

#[tokio::test]
async fn tool_arg_fragments_surface_as_tool_call_deltas() {
    // Round 1 streams a tool call's args in two fragments then the assembled
    // call; round 2 (after the tool result) ends the turn.
    let rounds = vec![
        vec![
            LlmEvent::ToolCallDelta {
                id: "call_1".into(),
                name: "edit".into(),
                delta: r#"{"path":"#.into(),
            },
            LlmEvent::ToolCallDelta {
                id: "call_1".into(),
                name: "edit".into(),
                delta: r#""a.rs"}"#.into(),
            },
            LlmEvent::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "edit".into(),
                input: r#"{"path":"a.rs"}"#.into(),
            }),
            LlmEvent::Finish {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage::default(),
            },
        ],
        vec![
            LlmEvent::Text("done".into()),
            LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            },
        ],
    ];

    let cfg = EngineConfig {
        llm_factory: std::sync::Arc::new(move || {
            Box::new(EventScriptLlm::new(clone_rounds(&rounds))) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, |_, _| "ok".to_string());

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;

    // Both arg fragments surfaced, in order, correlated to the call's request_id.
    let deltas: Vec<(&str, &str, &str)> = events
        .iter()
        .filter_map(|ev| match ev {
            OutEvent::ToolCallDelta {
                request_id,
                tool,
                delta,
                ..
            } => Some((request_id.as_str(), tool.as_str(), delta.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![
            ("call_1", "edit", r#"{"path":"#),
            ("call_1", "edit", r#""a.rs"}"#),
        ],
    );

    // The assembled call still drives the round-trip: a `ToolExec` with the
    // full input, correlated by the same request_id, follows the deltas.
    let exec = events
        .iter()
        .find_map(|ev| match ev {
            OutEvent::ToolExec {
                request_id, input, ..
            } => Some((request_id.as_str(), input.as_str())),
            _ => None,
        })
        .expect("a ToolExec for the assembled call");
    assert_eq!(exec, ("call_1", r#"{"path":"a.rs"}"#));

    // Deltas precede the assembled ToolExec in the stream.
    let first_delta = events
        .iter()
        .position(|ev| matches!(ev, OutEvent::ToolCallDelta { .. }))
        .unwrap();
    let exec_pos = events
        .iter()
        .position(|ev| matches!(ev, OutEvent::ToolExec { .. }))
        .unwrap();
    assert!(first_delta < exec_pos, "deltas must stream before ToolExec");
}

/// `EngineConfig::llm_factory` mints a fresh backend per session, so the round
/// script must be cloned into each one.
fn clone_rounds(rounds: &[Vec<LlmEvent>]) -> Vec<Vec<LlmEvent>> {
    rounds.to_vec()
}
