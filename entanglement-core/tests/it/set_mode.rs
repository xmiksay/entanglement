//! Permission mode as a session axis (ADR-0207, stage 3): an `InMsg::SetMode`
//! is carried opaquely — core validates nothing about the name — always
//! confirms with `OutEvent::ModeChanged`, and is deferred (stashed) while a
//! turn is live just like `SetAgent`/`SetGeneration`.
//!
//! The model-visible effect is a notice appended as the **last** message of
//! every request, rebuilt fresh from `Session::mode` each round — never
//! pushed into persisted `Context` (see `session::mode::mode_notice`'s doc
//! for why a persisted push would desync live vs. replayed history: a
//! session's very first `Prompt` always folds at the very first log record on
//! replay, so anything meant to precede it live cannot be reconstructed after
//! it without an ordering mismatch).

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, OutEvent, SessionId,
};
use std::sync::{Arc, Mutex};

/// Every request's full message list, in order.
type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

struct RecordingLlm {
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // `trailing_notice` now carries the mode notice out of band from
        // `messages` (the prompt-cache fix). Recorded as a synthesized final
        // message so this file's existing "last message is the notice"
        // assertions keep testing the same observable behavior.
        let mut recorded = req.messages.to_vec();
        if let Some(notice) = &req.trailing_notice {
            recorded.push(Message::user(notice.clone()));
        }
        self.seen.lock().unwrap().push(recorded);
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

fn recording_factory(seen: &Seen) -> entanglement_core::LlmFactory {
    let seen = seen.clone();
    Arc::new(move || Box::new(RecordingLlm { seen: seen.clone() }) as Box<dyn Llm>)
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

fn is_mode_changed(e: &OutEvent) -> bool {
    matches!(e, OutEvent::ModeChanged { .. })
}

#[tokio::test]
async fn session_start_announces_the_default_mode() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = EngineConfig {
        llm_factory: recording_factory(&seen),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();

    let ev = recv_until(&mut sub, is_mode_changed).await;
    let OutEvent::ModeChanged { mode, .. } = ev else {
        unreachable!()
    };
    assert_eq!(mode, "build", "DEFAULT_MODE is `build` (ADR-0207)");
}

#[tokio::test]
async fn every_request_carries_a_trailing_notice_reflecting_the_current_mode() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = EngineConfig {
        llm_factory: recording_factory(&seen),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(sid.clone(), "first prompt"))
        .await
        .unwrap();
    recv_until(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;

    // The first-ever round already carries the notice — session start, not
    // just a later switch — appended *after* the real prompt (never rewriting
    // anything earlier, never a separate pre-existing `ctx` entry).
    {
        let requests = seen.lock().unwrap();
        let first = requests.first().expect("a request was recorded");
        assert_eq!(
            first.last().map(Message::text).as_deref(),
            Some("[mode: build]"),
            "the mode notice must be the last message of the request: {first:?}"
        );
        let prompt_pos = first
            .iter()
            .position(|m| m.text() == "first prompt")
            .expect("the real prompt must appear in the request's messages");
        assert!(
            prompt_pos < first.len() - 1,
            "the notice must come after the prompt it follows, never before: {first:?}"
        );
    }

    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "research".into(),
        })
        .await
        .unwrap();
    recv_until(&mut sub, is_mode_changed).await;

    holly
        .send(InMsg::prompt(sid.clone(), "second prompt"))
        .await
        .unwrap();
    recv_until(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;

    // The switch is reflected on the very next request, still trailing.
    let requests = seen.lock().unwrap().clone();
    let last = requests.last().expect("a request was recorded");
    assert_eq!(
        last.last().map(Message::text).as_deref(),
        Some("[mode: research]"),
        "the notice must reflect the switched-to mode: {last:?}"
    );
}

/// An `Llm` that sleeps before returning, so a mid-turn `SetMode` reliably
/// lands in the inbox before the turn's stream resolves.
struct SlowLlm {
    delay: Duration,
}

#[async_trait]
impl Llm for SlowLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(self.delay).await;
        Ok(stream_from_response(LlmResponse {
            text: "turn reply".into(),
            tool_calls: vec![],
        }))
    }
}

#[tokio::test]
async fn set_mode_during_a_live_turn_is_deferred_until_it_ends() {
    let delay = Duration::from_millis(150);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || Box::new(SlowLlm { delay }) as Box<dyn Llm>),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    // Land inside the streaming delay window, before the turn resolves.
    tokio::time::sleep(Duration::from_millis(20)).await;
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "plan".into(),
        })
        .await
        .unwrap();

    // The live turn's own Done must land before the SetMode-triggered
    // ModeChanged — it was stashed, not applied concurrently. (The
    // session-start ModeChanged for `build` lands first, ahead of Done too;
    // only a *second* ModeChanged would indicate the switch landed early.)
    let mut events: VecDeque<OutEvent> = VecDeque::new();
    let mut mode_changed_before_done = 0;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out")
            .expect("event stream closed");
        let is_done = matches!(ev, OutEvent::Done { .. });
        if is_mode_changed(&ev) {
            mode_changed_before_done += 1;
        }
        events.push_back(ev);
        if is_done {
            break;
        }
    }
    assert_eq!(
        mode_changed_before_done, 1,
        "only the session-start ModeChanged may land before Done, not the SetMode one: {events:?}"
    );

    // The stashed command applies once the turn ends.
    let ev = recv_until(&mut sub, is_mode_changed).await;
    let OutEvent::ModeChanged { mode, .. } = ev else {
        unreachable!()
    };
    assert_eq!(mode, "plan");
}

#[tokio::test]
async fn resumed_session_reconstructs_the_same_mode() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = EngineConfig {
        llm_factory: recording_factory(&seen),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("resume-mode");
    let mut sub = holly.subscribe();

    // A minimal log: session start under the default mode, then a live
    // switch to `research` — the state a resumed process must land back in.
    let log = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                agent: "build".into(),
                model: None,
                root: true,
                ts: 0,
                user: None,
            },
        ),
        (
            None,
            OutEvent::ModeChanged {
                session: sid.clone(),
                mode: "build".into(),
            },
        ),
        (
            Some(InMsg::SetMode {
                session: sid.clone(),
                mode: "research".into(),
            }),
            OutEvent::ModeChanged {
                session: sid.clone(),
                mode: "research".into(),
            },
        ),
    ];

    holly.resume(sid.clone(), log).await.unwrap();

    // `session_loop` re-announces the current mode unconditionally at start
    // (mirroring `AgentChanged`) — replay must have landed `s.mode` back on
    // `research`, not the session's starting `build`.
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ModeChanged { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::ModeChanged { mode, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        mode, "research",
        "a resumed session must land in the same mode it was switched to before hibernation"
    );

    // And the reconstructed mode reaches the model on the resumed session's
    // very next request, exactly like a never-hibernated control would.
    holly
        .send(InMsg::prompt(sid.clone(), "continue"))
        .await
        .unwrap();
    recv_until(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;
    let requests = seen.lock().unwrap().clone();
    let last = requests.last().expect("a request was recorded");
    assert_eq!(
        last.last().map(Message::text).as_deref(),
        Some("[mode: research]"),
    );
}

/// ADR-0207 §6: mode applies to the whole spawn sub-tree — a spawned child
/// starts under its parent's *current* mode, not `DEFAULT_MODE`.
#[tokio::test]
async fn spawned_child_inherits_the_parents_live_mode() {
    let holly = Holly::spawn(EngineConfig::default());
    let parent = SessionId::new("parent-mode");
    let child = SessionId::new("child-mode");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(parent.clone(), "hi"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == parent),
    )
    .await;
    holly
        .send(InMsg::SetMode {
            session: parent.clone(),
            mode: "research".into(),
        })
        .await
        .unwrap();
    recv_until(&mut sub, |e| {
        matches!(e, OutEvent::ModeChanged { session, mode, .. } if *session == parent && mode == "research")
    })
    .await;

    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: "subtask".into(),
            user: None,
        })
        .await
        .unwrap();
    // The child's own session-start `ModeChanged` (unconditional, mirroring
    // `AgentChanged`) must already carry the inherited mode, not `build`.
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ModeChanged { session, .. } if *session == child),
    )
    .await;
    let OutEvent::ModeChanged { mode, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        mode, "research",
        "a spawned child must inherit its parent's live mode, not DEFAULT_MODE"
    );
}

/// ADR-0207 §6: a mode change reaches every live descendant, not just the
/// target session — mirroring how `CloseSession`/`HibernateSession` already
/// cascade over the spawn sub-tree.
#[tokio::test]
async fn set_mode_cascades_to_live_descendants() {
    let holly = Holly::spawn(EngineConfig::default());
    let parent = SessionId::new("parent-cascade");
    let child = SessionId::new("child-cascade");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::prompt(parent.clone(), "hi"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == parent),
    )
    .await;
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: "subtask".into(),
            user: None,
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == child),
    )
    .await;

    holly
        .send(InMsg::SetMode {
            session: parent.clone(),
            mode: "plan".into(),
        })
        .await
        .unwrap();

    // Both the target and the cascaded descendant announce `plan` — order
    // between the two is an implementation detail (the cascade reaches the
    // descendant before the target's own switch falls through the ordinary
    // routing path), so collect until both are seen rather than assuming one
    // precedes the other.
    let mut parent_switched = false;
    let mut child_switched = false;
    while !parent_switched || !child_switched {
        let ev = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for both ModeChanged(plan) events")
            .expect("event stream closed");
        if let OutEvent::ModeChanged { session, mode, .. } = &ev {
            if mode == "plan" && *session == parent {
                parent_switched = true;
            }
            if mode == "plan" && *session == child {
                child_switched = true;
            }
        }
    }
}
