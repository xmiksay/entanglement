//! Resuming a session must re-announce its `predecessor` lineage correctly
//! (#415, ADR-0110). `session_loop` used to emit the resumed session's
//! re-announced `SessionStarted` with the raw `predecessor` parameter, which
//! `Holly`'s `Resume` handling always passes as `None` (so it can't clobber
//! the value `Session::replay` already reconstructed from the log). That
//! blanked-out event then got persisted, so replaying the log a *second* time
//! folded the wrong (later, `None`) `SessionStarted` last and lost the
//! predecessor for good.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, OutEvent, SessionId};

#[tokio::test]
async fn resumed_successor_reannounces_its_predecessor() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();

    let source = SessionId::new("compact-source");
    let successor = SessionId::new("compact-successor");

    // A minimal log for a compaction successor (ADR-0110): a root with no
    // parent, recording the source it succeeds.
    let records = vec![(
        None,
        OutEvent::SessionStarted {
            session: successor.clone(),
            parent: None,
            predecessor: Some(source.clone()),
            profile: "build".into(),
            model: None,
            root: true,
            ts: 0,
        },
    )];

    holly.resume(successor.clone(), records).await.unwrap();

    let announced = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match sub.recv().await {
                Ok(ev) if matches!(&ev, OutEvent::SessionStarted { session, .. } if *session == successor) => {
                    return ev;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event stream closed before SessionStarted"),
            }
        }
    })
    .await
    .expect("timed out waiting for the resumed SessionStarted");

    let OutEvent::SessionStarted { predecessor, .. } = announced else {
        unreachable!()
    };
    assert_eq!(
        predecessor,
        Some(source),
        "the resumed session's re-announced SessionStarted must carry the \
         predecessor reconstructed from the log, not blank it out"
    );
}
