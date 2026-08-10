//! `InMsg::Spawn`'s `sponsored` flag round-trips onto `SessionStarted`/
//! `SessionInfo` (#626): a head disambiguates `AgentState::WaitingAgent`'s two
//! callers — a plain blocking `agent`/`agent_send` sub-agent wait vs. a
//! sponsored `propose_plan` build child (ADR-0138) — by this flag rather than
//! engine-side sponsorship bookkeeping, which stays runtime-internal
//! (`SpawnGuard`). Covers both a live spawn and — mirroring
//! `resume_predecessor.rs`'s "resumed takes precedence" regression — a
//! resumed session re-announcing the flag it was replayed with.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};

async fn recv_session_started(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    target: &SessionId,
) -> OutEvent {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match sub.recv().await {
                Ok(ev)
                    if matches!(&ev, OutEvent::SessionStarted { session, .. } if session == target) =>
                {
                    return ev;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event stream closed before SessionStarted"),
            }
        }
    })
    .await
    .expect("timed out waiting for SessionStarted")
}

#[tokio::test]
async fn a_sponsored_spawn_announces_sponsored_true() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let parent = SessionId::new("plan");
    let child = SessionId::new("build-child");

    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent),
            predecessor: None,
            agent: "build".into(),
            prompt: "implement the plan".into(),
            user: None,
            sponsored: true,
        })
        .await
        .unwrap();

    let OutEvent::SessionStarted { sponsored, .. } = recv_session_started(&mut sub, &child).await
    else {
        unreachable!()
    };
    assert!(
        sponsored,
        "a propose_plan handoff spawn must announce sponsored: true"
    );
}

#[tokio::test]
async fn a_plain_spawn_announces_sponsored_false() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let parent = SessionId::new("root");
    let child = SessionId::new("sub-agent");

    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent),
            predecessor: None,
            agent: "build".into(),
            prompt: "look into this".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();

    let OutEvent::SessionStarted { sponsored, .. } = recv_session_started(&mut sub, &child).await
    else {
        unreachable!()
    };
    assert!(
        !sponsored,
        "a plain agent-tool spawn must never announce sponsored: true"
    );
}

#[tokio::test]
async fn resumed_sponsored_child_reannounces_sponsored_true() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let parent = SessionId::new("plan");
    let child = SessionId::new("build-child");

    let records = vec![(
        None,
        OutEvent::SessionStarted {
            session: child.clone(),
            parent: Some(parent),
            predecessor: None,
            profile: "build".into(),
            model: None,
            root: false,
            ts: 0,
            user: None,
            sponsored: true,
        },
    )];

    holly.resume(child.clone(), records).await.unwrap();

    let OutEvent::SessionStarted { sponsored, .. } = recv_session_started(&mut sub, &child).await
    else {
        unreachable!()
    };
    assert!(
        sponsored,
        "a resumed sponsored build child's re-announced SessionStarted must not \
         blank out sponsored, mirroring the predecessor/user resumed-takes-\
         precedence rule"
    );
}
