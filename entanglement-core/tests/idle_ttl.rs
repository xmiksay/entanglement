//! Optional idle-TTL auto-hibernation (#363, ADR-0090): the supervisor
//! auto-hibernates a **settled** root session — and its whole spawn sub-tree —
//! after `EngineConfig::idle_ttl` of continuous idleness, reusing the existing
//! `HibernateSession` mechanism (#318, ADR-0077) so resume works identically.
//!
//! Acceptance (issue #363):
//! - `idle_ttl: None` (the default) never auto-hibernates — behavior identical
//!   to before the feature existed;
//! - a settled session auto-hibernates once idle past the TTL, with no manual
//!   `Holly::hibernate` call;
//! - a session mid-turn, or parked on a tool/approval/question result, is never
//!   auto-hibernated, however long the TTL has elapsed;
//! - a parked child pins its whole ancestry live — the settled root is not
//!   auto-hibernated while a spawned sub-agent is still parked.
//!
//! Every test drives tokio's *paused* virtual clock (`start_paused = true`)
//! rather than a real wall-clock sleep: with time paused, the runtime
//! auto-advances to the next pending timer once every task is otherwise idle,
//! so waiting out a multi-minute `idle_ttl` costs milliseconds of real time,
//! not minutes (same "no wall-clock waits" discipline as the resume tests,
//! applied via tokio's own clock rather than a bespoke one).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId,
};

/// Scripted responses drawn in call order; an empty script answers every round
/// plainly (ends the turn immediately).
type Responses = Arc<Mutex<VecDeque<LlmResponse>>>;

struct ScriptedLlm {
    responses: Responses,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
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

/// An LLM whose stream connects but never yields — used to hold a turn
/// actively streaming (mid-turn) or, combined with an unresolved tool call, to
/// hold a turn parked, for as long as a test needs.
struct StalledLlm;

#[async_trait]
impl Llm for StalledLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        Ok(Box::pin(futures::stream::pending()))
    }
}

fn engine(responses: Vec<LlmResponse>, idle_ttl: Option<Duration>) -> Holly {
    let responses: Responses = Arc::new(Mutex::new(responses.into()));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: responses.clone(),
            }) as Box<dyn Llm>
        }),
        idle_ttl,
        ..EngineConfig::default()
    };
    Holly::spawn(cfg)
}

/// Wait for the first event matching `pred`. A generous virtual timeout: under
/// a paused clock this resolves in milliseconds of real time regardless of how
/// large it is, so it comfortably outlives any `idle_ttl`/sweep-period this
/// file exercises.
async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3600), sub.recv())
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

/// Assert no event matching `pred` arrives within `within` (virtual time).
async fn assert_never(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    within: Duration,
    pred: impl Fn(&OutEvent) -> bool,
) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        match tokio::time::timeout(remaining, sub.recv()).await {
            Ok(Ok(ev)) => assert!(!pred(&ev), "unexpected event: {ev:?}"),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(_)) => return,
            Err(_) => return,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn no_idle_ttl_never_auto_hibernates() {
    // The default (`idle_ttl: None`) must behave exactly as before the
    // feature existed: no sweep runs at all, no matter how long a settled
    // session sits idle.
    let holly = engine(vec![], None);
    let sid = SessionId::new("no-ttl");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;

    assert_never(
        &mut sub,
        Duration::from_secs(3600),
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn settled_session_auto_hibernates_after_idle_ttl() {
    let ttl = Duration::from_secs(120);
    let holly = engine(vec![], Some(ttl));
    let sid = SessionId::new("settled");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;

    // No manual `Holly::hibernate` call — the sweep alone evicts it.
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;

    // Resumable exactly like a manual hibernate (#318): the id answers again.
    holly.resume(sid.clone(), vec![]).await.unwrap();
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

#[tokio::test(start_paused = true)]
async fn mid_stream_session_never_auto_hibernates() {
    // A turn stuck actively streaming (StalledLlm never yields) must never be
    // auto-hibernated, however long the TTL has elapsed — a timer must not
    // cancel live work (stricter than manual `HibernateSession`, which does
    // stop-then-hibernate).
    let ttl = Duration::from_secs(60);
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(StalledLlm) as Box<dyn Llm>),
        idle_ttl: Some(ttl),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("mid-stream");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    recv_until(&mut sub, |e| {
        matches!(e, OutEvent::Status { session, state, .. }
            if *session == sid && *state == entanglement_core::AgentState::Thinking)
    })
    .await;

    assert_never(
        &mut sub,
        ttl * 10,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn parked_session_never_auto_hibernates() {
    // A turn parked waiting on a tool result (the approval-wait shape) is
    // "mid-turn" from core's point of view (`Session::turn.is_some()`) exactly
    // like an active stream, so the same rule applies.
    let ttl = Duration::from_secs(60);
    let holly = engine(
        vec![LlmResponse {
            text: String::new(),
            tool_calls: vec![entanglement_core::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                input: "{}".into(),
                provider_meta: None,
            }],
        }],
        Some(ttl),
    );
    let sid = SessionId::new("parked");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "read file"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ToolExec { request_id, .. } if request_id == "call_1"),
    )
    .await;

    assert_never(
        &mut sub,
        ttl * 10,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn parked_child_pins_settled_root_live() {
    // Idleness is judged per root: a settled root whose spawned child is still
    // parked must not be auto-hibernated, even though the root itself has been
    // idle past the TTL.
    let ttl = Duration::from_secs(120);
    // Scripted responses are drawn from one queue shared by every session this
    // engine spawns (in call order) — the root's "hi" turn is the first
    // `stream()` call, so its plain reply must come first; the tool call is
    // reached only once the child's turn streams (call order matches send
    // order because the test waits for the root's `Done` before spawning).
    let holly = engine(
        vec![
            LlmResponse {
                text: "root-done".into(),
                tool_calls: vec![],
            },
            LlmResponse {
                text: String::new(),
                tool_calls: vec![entanglement_core::ToolCall {
                    id: "child_call".into(),
                    name: "read".into(),
                    input: "{}".into(),
                    provider_meta: None,
                }],
            },
        ],
        Some(ttl),
    );
    let root = SessionId::new("root");
    let child = SessionId::new("child");
    let mut sub = holly.subscribe();

    // Settle the root first.
    holly.send(InMsg::prompt(root.clone(), "hi")).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == root),
    )
    .await;

    // Spawn a child that immediately parks on an unresolved tool call.
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: root.clone(),
            agent: "build".into(),
            prompt: "do the subtask".into(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ToolExec { request_id, .. } if request_id == "child_call"),
    )
    .await;

    // The root is settled and past its own TTL window, but the child pins the
    // whole tree live.
    assert_never(&mut sub, ttl * 10, |e| {
        matches!(e, OutEvent::SessionHibernated { session, .. } if *session == root || *session == child)
    })
    .await;
}
