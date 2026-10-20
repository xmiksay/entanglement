//! A compaction lineage is linear (ADR-0243): a session has at most one
//! successor, so no session's lineage head ever moves under the runtime,
//! which keys a running script's overlay epochs by it.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};

fn spawn(session: &SessionId, predecessor: Option<&SessionId>) -> InMsg {
    InMsg::Spawn {
        session: session.clone(),
        parent: None,
        predecessor: predecessor.cloned(),
        agent: "general".into(),
        prompt: "continue".into(),
        user: None,
        model: None,
        mode: None,
        workdir: None,
    }
}

/// Read `sub` until `hit`, panicking on a timeout.
async fn until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    hit: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match sub.recv().await {
                Ok(ev) if hit(&ev) => return ev,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("event stream closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out")
}

/// A successor's minimal log: its own start, naming its predecessor.
fn log(session: &SessionId, predecessor: &SessionId) -> Vec<(Option<InMsg>, OutEvent)> {
    vec![(
        None,
        OutEvent::SessionStarted {
            session: session.clone(),
            parent: None,
            predecessor: Some(predecessor.clone()),
            agent: "general".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        },
    )]
}

fn started(ev: &OutEvent, id: &SessionId) -> bool {
    matches!(ev, OutEvent::SessionStarted { session, .. } if session == id)
}

#[tokio::test]
async fn a_second_successor_of_one_session_is_refused() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let [p, s1, s2] = ["p", "s1", "s2"].map(SessionId::new);
    holly.send(spawn(&p, None)).await.unwrap();
    until(&mut sub, |ev| started(ev, &p)).await;
    holly.send(spawn(&s1, Some(&p))).await.unwrap();
    until(&mut sub, |ev| started(ev, &s1)).await;

    holly.send(spawn(&s2, Some(&p))).await.unwrap();
    let refused = until(&mut sub, |ev| {
        matches!(ev, OutEvent::Error { session, .. } if *session == s2) || started(ev, &s2)
    })
    .await;
    let OutEvent::Error { message, .. } = refused else {
        panic!("the second successor started: {refused:?}")
    };
    assert!(message.contains("second successor"), "{message}");
    until(&mut sub, |ev| {
        matches!(ev, OutEvent::Error { session, message, .. }
            if *session == p && message.contains("second successor"))
    })
    .await;
    assert_eq!(holly.lineage_head(&s1), p, "s1's lineage head stays");
    assert_eq!(holly.successor_of(&p), Some(s1));
    assert!(holly.session_info(&s2).is_none(), "nothing recorded for s2");
}

/// A resume re-learning an older log's lineage keeps the successor this
/// process already recorded: the head of the live successor never moves.
#[tokio::test]
async fn a_resumed_older_successor_does_not_take_over_the_lineage() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let [p, s1, old] = ["p", "s1", "old"].map(SessionId::new);
    holly.send(spawn(&p, None)).await.unwrap();
    until(&mut sub, |ev| started(ev, &p)).await;
    holly.send(spawn(&s1, Some(&p))).await.unwrap();
    until(&mut sub, |ev| started(ev, &s1)).await;

    holly.resume(old.clone(), log(&old, &p)).await.unwrap();
    until(&mut sub, |ev| started(ev, &old)).await;
    assert_eq!(holly.successor_of(&p), Some(s1.clone()));
    assert_eq!(holly.lineage_head(&s1), p);
}

/// A compacted-away session is not resumable (ADR-0243): resuming the
/// successor taught this process the edge, so a later resume of the
/// predecessor is refused, naming the successor, and leaves the successor's
/// lineage head where it was.
#[tokio::test]
async fn a_compacted_away_session_is_not_resumable() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let [p, s1, s2] = ["p", "s1", "s2"].map(SessionId::new);
    holly.resume(s2.clone(), log(&s2, &s1)).await.unwrap();
    until(&mut sub, |ev| started(ev, &s2)).await;
    assert_eq!(holly.lineage_head(&s2), s1);

    holly.resume(s1.clone(), log(&s1, &p)).await.unwrap();
    let refused = until(&mut sub, |ev| {
        matches!(ev, OutEvent::Error { session, .. } if *session == s1) || started(ev, &s1)
    })
    .await;
    let OutEvent::Error { message, .. } = refused else {
        panic!("the compacted-away session resumed: {refused:?}")
    };
    assert!(message.contains("compacted into `s2`"), "{message}");
    assert_eq!(holly.lineage_head(&s2), s1, "the head did not move");
    assert!(holly.session_info(&s1).is_none());
}
