//! Supervisor session-lifecycle edge cases (issue #105): a failed replay must
//! not become a silent black hole, a closed id must not be resurrected, and a
//! `Resume` of a live id must not orphan the running task. These drive the
//! `Holly` actor end-to-end — the routing/replay logic behind the three bugs is
//! private to the supervisor, so the public inbox/outbox is the seam.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, ProfileRegistry, SessionId};

/// Wait for the first event matching `pred`, tolerating broadcast lag and events
/// for other sessions. Panics on timeout so a black hole (no event) fails loudly.
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

/// An engine config whose replay always fails: an empty profile registry has no
/// `build` profile, so `Session::replay` errors before folding any record.
fn cfg_without_build_profile() -> EngineConfig {
    EngineConfig {
        profiles: ProfileRegistry::default(),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn failed_replay_surfaces_error_and_leaves_no_ghost_session() {
    // Bug 1: a session whose replay fails used to be registered anyway, showing
    // in `ListSessions` while every routed `Prompt` hit a closed channel and
    // vanished. Now the failure emits an `Error` and claims no id.
    let holly = Holly::spawn(cfg_without_build_profile());
    let sid = SessionId::new("resume-fail");
    let mut sub = holly.subscribe();

    holly.resume(sid.clone(), vec![]).await.unwrap();

    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Error { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::Error { message, .. } = ev else {
        unreachable!()
    };
    assert!(
        message.contains("failed to resume"),
        "failed replay should say so; got {message:?}"
    );

    // The dead id must not linger in the live-session directory.
    let corr = SessionId::new("query");
    holly
        .send(InMsg::ListSessions {
            session: corr.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { session, .. } if *session == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    assert!(
        !sessions.iter().any(|i| i.session == sid),
        "a failed-replay session must not appear in the list; got {sessions:?}"
    );
}

#[tokio::test]
async fn closed_id_is_not_resurrected_by_a_later_prompt() {
    // Bug 2: a `Prompt` racing behind `CloseSession` used to lazily respawn a
    // blank session under the retired id — a head that saw `SessionEnded` would
    // then see a fresh `SessionStarted`. Now the id is a tombstone: refused.
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;
    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionEnded { session, .. } if *session == sid),
    )
    .await;

    // Re-prompt the retired id: must be refused, never a second start.
    let mut sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "again"))
        .await
        .unwrap();

    let ev = recv_until(
        &mut sub2,
        |e| matches!(e, OutEvent::Error { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::Error { message, .. } = ev else {
        unreachable!()
    };
    assert!(
        message.contains("closed"),
        "refusal should mention the id is closed; got {message:?}"
    );

    // Prove no fresh `SessionStarted` was emitted for the retired id: a
    // `ListSessions` round-trip drains the window, and the id stays absent.
    let corr = SessionId::new("query");
    holly
        .send(InMsg::ListSessions {
            session: corr.clone(),
        })
        .await
        .unwrap();
    let mut saw_restart = false;
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub2.recv())
            .await
            .expect("timed out waiting for the ListSessions reply");
        match recv {
            Ok(OutEvent::SessionStarted { session, .. }) if session == sid => saw_restart = true,
            Ok(OutEvent::SessionList {
                session, sessions, ..
            }) if session == corr => {
                assert!(
                    !sessions.iter().any(|i| i.session == sid),
                    "closed id must stay gone; got {sessions:?}"
                );
                break;
            }
            _ => {}
        }
    }
    assert!(
        !saw_restart,
        "a closed id must not emit a second SessionStarted"
    );
}

#[tokio::test]
async fn resume_of_a_live_id_is_refused() {
    // Bug 3: `Resume` had no liveness guard, so `sessions.insert` overwrote the
    // sender and the running task's channel closed mid-flight. Now it's refused
    // like a duplicate `Spawn`.
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;

    // The session is still live (idle, awaiting the next command). A `Resume`
    // for it must be refused rather than orphaning the task.
    let mut sub2 = holly.subscribe();
    holly.resume(sid.clone(), vec![]).await.unwrap();

    let ev = recv_until(
        &mut sub2,
        |e| matches!(e, OutEvent::Error { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::Error { message, .. } = ev else {
        unreachable!()
    };
    assert!(
        message.contains("already-live"),
        "refusal should mention the id is already live; got {message:?}"
    );

    // The original session is unharmed: it still answers a follow-up prompt.
    holly
        .send(InMsg::prompt(sid.clone(), "still there?"))
        .await
        .unwrap();
    recv_until(
        &mut sub2,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == sid),
    )
    .await;
}
