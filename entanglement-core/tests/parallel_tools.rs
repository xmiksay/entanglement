//! Parked-turn batch semantics (#270, ADR-0061): a round ending in tool calls
//! emits the whole batch as `ToolExec` up front and parks as explicit
//! `TurnState`; results resolve in any order, duplicates and unknown ids are
//! dropped, `Stop` cancels the parked turn without killing the session, and a
//! `Prompt` sent while parked folds into the live turn (ADR-0058).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmSession,
    LlmStream, Message, OutEvent, SessionId, ToolCall,
};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: "{}".into(),
    }
}

/// Scripted LLM that also records each request's messages, so a test can
/// assert what context a later round actually saw.
struct RecordingLlm {
    responses: Mutex<Vec<LlmResponse>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl RecordingLlm {
    fn new(mut responses: Vec<LlmResponse>, seen: Arc<Mutex<Vec<Vec<Message>>>>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
            seen,
        }
    }
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let resp = {
            let mut responses = self.responses.lock().unwrap();
            responses.pop().unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            })
        };
        Ok(stream_from_response(resp))
    }
}

fn engine(responses: Vec<LlmResponse>) -> (Holly, Arc<Mutex<Vec<Vec<Message>>>>) {
    let seen: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_factory = seen.clone();
    let responses = Arc::new(responses);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(RecordingLlm::new(
                (*responses).clone(),
                seen_factory.clone(),
            )))
        }),
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), seen)
}

/// Drain events for `sid` until the deadline; returns them in arrival order.
async fn collect_for(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dur: Duration,
) -> Vec<OutEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + dur;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() == sid {
            events.push(ev);
        }
    }
    events
}

/// Wait until `n` `ToolExec` events for `sid` have arrived; returns their
/// request ids in emit order.
async fn await_tool_execs(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    n: usize,
) -> Vec<String> {
    let mut ids = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while ids.len() < n {
        let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await else {
            break;
        };
        if let OutEvent::ToolExec {
            session,
            request_id,
            ..
        } = ev
        {
            if session == *sid {
                ids.push(request_id);
            }
        }
    }
    ids
}

/// The whole batch is offered before any result is consumed, and results
/// resolve out of order: outputs surface in *arrival* order and the turn
/// continues to exactly one `Done`.
#[tokio::test]
async fn batch_emits_up_front_and_resolves_out_of_order() {
    let (holly, _) = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![call("a", "t_a"), call("b", "t_b"), call("c", "t_c")],
        },
        LlmResponse {
            text: "final".into(),
            tool_calls: vec![],
        },
    ]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let obs = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();

    // All three ToolExec arrive while zero results have been sent — the batch
    // is emitted up front, not one-at-a-time.
    let ids = await_tool_execs(&mut sub, &sid, 3).await;
    assert_eq!(ids, vec!["a", "b", "c"], "batch emitted in call order");

    // Answer out of order: c, a, b.
    for id in ["c", "a", "b"] {
        holly
            .send(InMsg::ToolResult {
                session: sid.clone(),
                request_id: id.into(),
                output: format!("out-{id}"),
            })
            .await
            .unwrap();
    }

    let events = collect_for(obs, &sid, Duration::from_millis(500)).await;
    let outputs: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { request_id, .. } => Some(request_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        outputs,
        vec!["c", "a", "b"],
        "outputs fold in arrival order"
    );
    let dones = events
        .iter()
        .filter(|e| matches!(e, OutEvent::Done { .. }))
        .count();
    assert_eq!(dones, 1, "turn completes exactly once");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "final")),
        "second round runs after the batch drains"
    );
}

/// Unknown and duplicate `ToolResult` ids are dropped without corrupting the
/// turn: the batch still drains exactly once and the follow-up round sees one
/// tool message per call.
#[tokio::test]
async fn duplicate_and_unknown_results_are_dropped() {
    let (holly, seen) = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![call("a", "t_a")],
        },
        LlmResponse {
            text: "final".into(),
            tool_calls: vec![],
        },
    ]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let obs = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    let ids = await_tool_execs(&mut sub, &sid, 1).await;
    assert_eq!(ids, vec!["a"]);

    for (id, out) in [("nope", "bogus"), ("a", "real"), ("a", "dup")] {
        holly
            .send(InMsg::ToolResult {
                session: sid.clone(),
                request_id: id.into(),
                output: out.into(),
            })
            .await
            .unwrap();
    }

    let events = collect_for(obs, &sid, Duration::from_millis(500)).await;
    let outputs: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, vec!["real"], "only the matching result surfaces");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, OutEvent::Done { .. }))
            .count(),
        1
    );

    // The second round's context carries exactly one tool message for `a`.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "exactly two LLM round-trips");
    let tool_msgs: Vec<&Message> = seen[1]
        .iter()
        .filter(|m| m.tool_call_id.as_deref() == Some("a"))
        .collect();
    assert_eq!(tool_msgs.len(), 1, "one tool message per resolved call");
    assert_eq!(tool_msgs[0].text, "real");
}

/// `Stop` while parked mid-batch cancels the turn (no `Done`) but keeps the
/// session alive with the committed assistant message and the already-arrived
/// output in context.
#[tokio::test]
async fn stop_while_parked_keeps_session_and_context() {
    let (holly, seen) = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![call("a", "t_a"), call("b", "t_b")],
        },
        LlmResponse {
            text: "after-stop".into(),
            tool_calls: vec![],
        },
    ]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let obs = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    let ids = await_tool_execs(&mut sub, &sid, 2).await;
    assert_eq!(ids.len(), 2);

    // One result lands, then the user cancels.
    holly
        .send(InMsg::ToolResult {
            session: sid.clone(),
            request_id: "a".into(),
            output: "out-a".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    // A late result for the cancelled call is stale and must be dropped.
    holly
        .send(InMsg::ToolResult {
            session: sid.clone(),
            request_id: "b".into(),
            output: "out-b".into(),
        })
        .await
        .unwrap();

    let events = collect_for(obs, &sid, Duration::from_millis(300)).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, OutEvent::Done { .. }))
            .count(),
        0,
        "cancelled turn must not complete"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { request_id, .. } if request_id == "b")),
        "late result after cancel is dropped"
    );

    // The session survives: a fresh prompt runs a new turn over the preserved
    // context (assistant tool-call message + out-a tool message).
    let obs2 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "again".into(),
        })
        .await
        .unwrap();
    let events = collect_for(obs2, &sid, Duration::from_millis(500)).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "after-stop")),
        "session stays alive after a parked-turn cancel"
    );
    let seen = seen.lock().unwrap();
    let second = seen.last().expect("second round-trip");
    assert!(
        second
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("a") && m.text == "out-a"),
        "already-arrived output survives the cancel in context"
    );
}

/// A `Prompt` sent while the turn is parked folds into the live turn (#182,
/// ADR-0058): the next round's request carries it, and only one `Done` fires.
#[tokio::test]
async fn prompt_while_parked_folds_into_live_turn() {
    let (holly, seen) = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![call("a", "t_a")],
        },
        LlmResponse {
            text: "final".into(),
            tool_calls: vec![],
        },
    ]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let obs = holly.subscribe();

    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    let ids = await_tool_execs(&mut sub, &sid, 1).await;
    assert_eq!(ids, vec!["a"]);

    // Steering arrives while parked — before the result.
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "steer".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::ToolResult {
            session: sid.clone(),
            request_id: "a".into(),
            output: "out-a".into(),
        })
        .await
        .unwrap();

    let events = collect_for(obs, &sid, Duration::from_millis(500)).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, OutEvent::Done { .. }))
            .count(),
        1,
        "steering folds into the live turn — one Done, not two"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "no third round-trip for the folded prompt");
    assert!(
        seen[1].iter().any(|m| m.text == "steer"),
        "the folded prompt reaches the model on the next round"
    );
}
