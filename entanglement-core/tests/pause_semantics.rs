//! `PauseSession`/`ResumeSession` (#516, ADR-0144): a middle ground between
//! `Stop` (destroys the in-flight round) and `HibernateSession` (evicts
//! memory). These drive `Holly` end-to-end — the seam is the public
//! inbox/outbox.
//!
//! Acceptance (issue #516):
//! - pause-while-parked: a batch's already-arrived `ToolResult`s still fold
//!   into `Context`, but the turn doesn't continue past a drained batch;
//! - resume-continues: `ResumeSession` continues the very same round with no
//!   new prompt;
//! - pause-then-Stop: `Stop` still cancels the turn, but a still-paused
//!   session reports `Paused`, not `Done`;
//! - pause-across-hibernate: hibernating a paused session bypasses the hold
//!   (Hibernate always wins), and the resumed session comes back unpaused.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentState, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, Message, OutEvent, SessionId, ToolCall,
};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: "{}".into(),
        provider_meta: None,
    }
}

type Seen = Arc<Mutex<Vec<Vec<Message>>>>;
/// Scripted responses drawn in call order, **shared** across every `Session`
/// object one engine builds (a resumed session after hibernate is a distinct
/// `Session`, with its own `llm_factory` call) — so a continuation across a
/// hibernate/resume boundary gets the *next* scripted response, not a
/// re-cloned script restarting from the top (mirrors `hibernate.rs`).
type Responses = Arc<Mutex<VecDeque<LlmResponse>>>;

struct RecordingLlm {
    responses: Responses,
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

fn engine(responses: Vec<LlmResponse>) -> (Holly, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let responses: Responses = Arc::new(Mutex::new(responses.into()));
    let seen2 = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                responses: responses.clone(),
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), seen)
}

async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

/// Drain events for `sid` for a bounded window; returns them in arrival order.
/// Used to assert *absence* (no `Done`/`TextDelta` fired while paused).
async fn collect_for(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    dur: Duration,
) -> Vec<OutEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + dur;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() == Some(sid) {
            events.push(ev);
        }
    }
    events
}

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

/// Pause-while-parked: the batch's already-arrived results still fold into
/// `Context` (both `ToolOutput`s surface), but the turn does not continue past
/// the drained batch — no third round-trip, no `Done` — until resumed.
#[tokio::test]
async fn pause_while_parked_holds_the_batch_but_still_folds_results() {
    let (holly, seen) = engine(vec![
        LlmResponse {
            text: String::new(),
            tool_calls: vec![call("a", "t_a"), call("b", "t_b")],
        },
        LlmResponse {
            text: "final".into(),
            tool_calls: vec![],
        },
    ]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let obs = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let ids = await_tool_execs(&mut sub, &sid, 2).await;
    assert_eq!(ids.len(), 2);

    holly
        .send(InMsg::PauseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, state, .. } if *session == sid && *state == AgentState::Paused),
    )
    .await;

    // Both results still resolve and fold while paused.
    holly
        .send(InMsg::tool_result(sid.clone(), "a", "out-a"))
        .await
        .unwrap();
    holly
        .send(InMsg::tool_result(sid.clone(), "b", "out-b"))
        .await
        .unwrap();

    let events = collect_for(obs, &sid, Duration::from_millis(400)).await;
    let outputs: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { request_id, .. } => Some(request_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, vec!["a", "b"], "both results fold while paused");
    assert!(
        !events.iter().any(|e| matches!(e, OutEvent::Done { .. })),
        "the turn must not continue past the drained batch while paused"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { .. })),
        "no next-round model request while paused"
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "only the first round ran");
}

/// `ResumeSession` continues the very same round with no new prompt: the
/// buffered next-round model request fires and the turn completes.
#[tokio::test]
async fn resume_continues_the_same_round_without_reprompting() {
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

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    assert_eq!(await_tool_execs(&mut sub, &sid, 1).await, vec!["a"]);

    holly
        .send(InMsg::PauseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, state, .. } if *session == sid && *state == AgentState::Paused),
    )
    .await;

    holly
        .send(InMsg::tool_result(sid.clone(), "a", "out-a"))
        .await
        .unwrap();
    // Give the drained-but-paused turn a moment to (not) continue.
    tokio::time::sleep(Duration::from_millis(150)).await;

    holly
        .send(InMsg::ResumeSession {
            session: sid.clone(),
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
        "resuming continues the same round to completion"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "final")),
        "the buffered next round runs on resume"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "exactly two round-trips total — no re-prompt"
    );
}

/// Pausing an idle session defers a new `Prompt` — no turn starts — until
/// `ResumeSession` lifts the hold, at which point the deferred prompt runs.
#[tokio::test]
async fn pause_while_idle_defers_new_prompt_until_resume() {
    let (holly, _seen) = engine(vec![LlmResponse {
        text: "final".into(),
        tool_calls: vec![],
    }]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::PauseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, state, .. } if *session == sid && *state == AgentState::Paused),
    )
    .await;

    let obs = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect_for(obs, &sid, Duration::from_millis(300)).await;
    assert!(
        events.is_empty(),
        "a prompt sent while paused must not start a turn; got {events:?}"
    );

    let obs2 = holly.subscribe();
    holly
        .send(InMsg::ResumeSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let events = collect_for(obs2, &sid, Duration::from_millis(500)).await;
    assert!(
        events.iter().any(|e| matches!(e, OutEvent::Done { .. })),
        "the deferred prompt runs once resumed; got {events:?}"
    );
}

/// `Stop` still cancels a paused, parked turn — but doesn't lift the pause: a
/// still-paused session reports `Paused`, not `Done`. A later `ResumeSession`
/// (now idle) settles it to `Done`.
#[tokio::test]
async fn pause_then_stop_cancels_turn_but_keeps_paused_state() {
    let (holly, _seen) = engine(vec![LlmResponse {
        text: String::new(),
        tool_calls: vec![call("a", "t_a")],
    }]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    assert_eq!(await_tool_execs(&mut sub, &sid, 1).await, vec!["a"]);

    holly
        .send(InMsg::PauseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, state, .. } if *session == sid && *state == AgentState::Paused),
    )
    .await;

    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, .. } if *session == sid),
    )
    .await;
    assert!(
        matches!(
            ev,
            OutEvent::Status {
                state: AgentState::Paused,
                ..
            }
        ),
        "Stop cancels the turn but must not silently lift the pause; got {ev:?}"
    );

    holly
        .send(InMsg::ResumeSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, .. } if *session == sid),
    )
    .await;
    assert!(
        matches!(
            ev,
            OutEvent::Status {
                state: AgentState::Done,
                ..
            }
        ),
        "resuming an idle (post-cancel) session settles it to Done; got {ev:?}"
    );
}

/// A stuck automation flooding a paused session with prompts must not grow
/// the deferred-command stash without bound (#556): past the cap, a prompt
/// is dropped and reported via `OutEvent::Error` instead of queuing forever.
#[tokio::test]
async fn pause_with_a_prompt_flood_reports_an_error_past_the_stash_cap() {
    let (holly, _seen) = engine(vec![LlmResponse {
        text: "final".into(),
        tool_calls: vec![],
    }]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::PauseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, state, .. } if *session == sid && *state == AgentState::Paused),
    )
    .await;

    let obs = holly.subscribe();
    // Comfortably past any reasonable cap — every one of these is stashable
    // (paused, idle turn), so a real cap must reject some of them.
    for _ in 0..200 {
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    }
    let events = collect_for(obs, &sid, Duration::from_millis(500)).await;
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::Error { message, .. } if message.contains("too many commands queued"))
        ),
        "flooding a paused session past the stash cap must surface an error; got {events:?}"
    );
}

/// Hibernating a paused session bypasses the hold (memory eviction always
/// wins, mirroring `Stop`): the pending call is re-offered on resume exactly
/// as an unpaused hibernate would, and the resumed session is not stuck —
/// resolving it completes the turn with no `ResumeSession` needed.
#[tokio::test]
async fn pause_across_hibernate_drops_the_hold_on_resume() {
    let (holly, _seen) = engine(vec![
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

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut log = Vec::new();
    let mut pending = Some(InMsg::prompt(sid.clone(), "go"));
    loop {
        let ev = recv_until(&mut sub, |e| e.session() == Some(&sid)).await;
        let parked = matches!(&ev, OutEvent::ToolExec { request_id, .. } if request_id == "a");
        log.push((pending.take(), ev));
        if parked {
            break;
        }
    }

    holly
        .send(InMsg::PauseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, state, .. } if *session == sid && *state == AgentState::Paused),
    )
    .await;

    holly.hibernate(sid.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;

    holly.resume(sid.clone(), log).await.unwrap();
    let reoffer = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ToolExec { session, request_id, .. } if *session == sid && request_id == "a"),
    )
    .await;
    assert!(matches!(reoffer, OutEvent::ToolExec { .. }));

    // Resolving it completes the turn with no `ResumeSession` — the resumed
    // session came back unpaused (pause is ephemeral, never persisted).
    holly
        .send(InMsg::tool_result(sid.clone(), "a", "out-a"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;
}
