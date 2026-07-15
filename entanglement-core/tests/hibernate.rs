//! Session hibernation (#318, ADR-0077): `HibernateSession` evicts a session's
//! in-memory state **without** tombstoning its id, so `Holly::resume` rebuilds it
//! from the embedder's event log exactly like the restart path. These drive the
//! `Holly` actor end-to-end — the seam is the public inbox/outbox.
//!
//! Acceptance (issue #318):
//! - hibernate → resume → continue preserves context (identical provider messages
//!   vs a never-hibernated control);
//! - hibernate mid-approval → resume re-offers the pending `ToolExec` (same
//!   `request_id`);
//! - a hibernated id holds no supervisor map entry (gone from `ListSessions`) yet
//!   stays resumable — unlike a closed id.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, OutEvent, SessionId, ToolCall,
};

/// Messages the provider saw on each round-trip, shared across every session an
/// engine builds so a test can assert the reconstructed context.
type Seen = Arc<Mutex<Vec<Vec<Message>>>>;
/// Scripted responses drawn in call order, **shared** across the pre-hibernate
/// and post-resume sessions (which are distinct `Session` objects on one engine),
/// so a resumed turn's continuation gets the *next* response, not a re-clone.
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
                text: "assistant-reply".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// An engine whose every session records the messages it's handed and streams the
/// given responses in call order (pass them in order of use; an empty script
/// defaults every round to a plain "assistant-reply").
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

/// Collect a faithful resume log for `sid`: every event up to and including the
/// first `Done`, with `prompt` tagged onto the first event (the way the runtime's
/// `pair_records` associates a prompt with the events it produced).
async fn record_turn(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    prompt: InMsg,
) -> Vec<(Option<InMsg>, OutEvent)> {
    let mut records = Vec::new();
    let mut pending = Some(prompt);
    loop {
        let ev = recv_until(sub, |e| e.session() == Some(sid)).await;
        let done = matches!(&ev, OutEvent::Done { .. });
        records.push((pending.take(), ev));
        if done {
            break;
        }
    }
    records
}

#[tokio::test]
async fn hibernate_then_resume_preserves_context_like_a_control() {
    // Control: one session runs two turns, never hibernated.
    let (control, control_seen) = engine(vec![]);
    let cid = SessionId::new("control");
    let mut csub = control.subscribe();
    control
        .send(InMsg::prompt(cid.clone(), "one"))
        .await
        .unwrap();
    recv_until(
        &mut csub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == cid),
    )
    .await;
    control
        .send(InMsg::prompt(cid.clone(), "two"))
        .await
        .unwrap();
    recv_until(
        &mut csub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == cid),
    )
    .await;
    let control_two_call = control_seen.lock().unwrap().last().cloned().unwrap();

    // Hibernate path: run turn one, capture its log, hibernate, resume, run two.
    let (holly, seen) = engine(vec![]);
    let sid = SessionId::new("hib");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    let log = record_turn(&mut sub, &sid, InMsg::prompt(sid.clone(), "one")).await;

    holly.hibernate(sid.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;

    holly.resume(sid.clone(), log).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Status { session, .. } if *session == sid),
    )
    .await;

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;
    let resumed_two_call = seen.lock().unwrap().last().cloned().unwrap();

    assert_eq!(
        resumed_two_call, control_two_call,
        "the resumed session must send the model the same context as a never-hibernated control"
    );
    assert_eq!(
        resumed_two_call.len(),
        3,
        "context is [user one, assistant reply, user two]; got {resumed_two_call:?}"
    );
}

#[tokio::test]
async fn hibernate_mid_approval_then_resume_reoffers_pending_call() {
    // Turn parks on a tool call (the approval wait), then hibernates before the
    // result arrives. Resume must re-offer the pending `ToolExec` with the same
    // `request_id` so the parked approval is recoverable, not lost.
    let (holly, _seen) = engine(vec![
        // First round: one tool call → the session parks.
        LlmResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                input: "{}".into(),
            }],
        },
        // After resolution the next round answers plainly and ends the turn.
        LlmResponse {
            text: "final".into(),
            tool_calls: vec![],
        },
    ]);
    let sid = SessionId::new("hib-approval");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "read file"))
        .await
        .unwrap();
    // Capture the log through the parked `ToolExec` (no `ToolResult` yet).
    let mut log = Vec::new();
    let mut pending = Some(InMsg::prompt(sid.clone(), "read file"));
    loop {
        let ev = recv_until(&mut sub, |e| e.session() == Some(&sid)).await;
        let parked = matches!(&ev, OutEvent::ToolExec { request_id, .. } if request_id == "call_1");
        log.push((pending.take(), ev));
        if parked {
            break;
        }
    }

    holly.hibernate(sid.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;

    holly.resume(sid.clone(), log).await.unwrap();

    // The resumed session re-offers call_1 as a fresh `ToolExec`.
    let reoffer = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ToolExec { session, request_id, .. } if *session == sid && request_id == "call_1"),
    )
    .await;
    let OutEvent::ToolExec { request_id, .. } = reoffer else {
        unreachable!()
    };
    assert_eq!(
        request_id, "call_1",
        "same request_id as the hibernated offer"
    );

    // A resolver answers it; the turn continues to Done.
    holly
        .send(InMsg::tool_result(sid.clone(), "call_1", "file contents"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;
}

/// An LLM whose stream connects but never yields — a stalled-but-connected
/// provider, so the session sits inside the streaming round.
struct StalledLlm;

#[async_trait]
impl Llm for StalledLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        Ok(Box::pin(futures::stream::pending()))
    }
}

#[tokio::test]
async fn hibernate_mid_stream_tears_down_and_stays_resumable() {
    // Stop-then-hibernate (ADR-0077): hibernating a session mid-stream unwinds the
    // in-flight round (the supervisor's sender-drop cancels it) and evicts the
    // session, emitting `SessionHibernated`. The uncommitted round is discarded —
    // exactly what replay does with a text-only tail — so the id stays resumable.
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(StalledLlm) as Box<dyn Llm>),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("hib-stream");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    // Wait until the turn is actually streaming (Status: Thinking) before evicting.
    recv_until(&mut sub, |e| {
        matches!(e, OutEvent::Status { session, state, .. }
            if *session == sid && *state == entanglement_core::AgentState::Thinking)
    })
    .await;

    holly.hibernate(sid.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;

    // The id is not tombstoned: a resume rebuilds it (empty log → fresh session).
    holly.resume(sid.clone(), vec![]).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
}

#[tokio::test]
async fn hibernated_id_leaves_no_map_entry_but_stays_resumable() {
    let (holly, _seen) = engine(vec![]);
    let sid = SessionId::new("hib-list");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    let log = record_turn(&mut sub, &sid, InMsg::prompt(sid.clone(), "hi")).await;

    holly.hibernate(sid.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;

    // The evicted id must be gone from the live-session directory (memory released).
    let corr = "q".to_string();
    holly
        .send(InMsg::ListSessions {
            correlation_id: corr.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { correlation_id, .. } if *correlation_id == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    assert!(
        !sessions.iter().any(|i| i.session == sid),
        "a hibernated id must hold no supervisor map entry; got {sessions:?}"
    );

    // Unlike a closed id, it is resumable: resume rebuilds it and it answers again.
    holly.resume(sid.clone(), log).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
    holly
        .send(InMsg::prompt(sid.clone(), "again"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;
}
