//! `Holly::shutdown` (#700): the engine stops and both broadcasts close
//! deterministically even while other `Holly` clones are still alive — the
//! normal teardown path no longer depends on every clone being dropped.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::common::collect_until_done;

/// Drain `rx` until it reports `Closed`, failing if that takes over a second.
async fn assert_closes<T: Clone>(rx: &mut broadcast::Receiver<T>) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match rx.recv().await {
                Err(RecvError::Closed) => break,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
            }
        }
    })
    .await
    .expect("broadcast did not close promptly after shutdown");
}

#[tokio::test]
async fn shutdown_closes_both_broadcasts_while_a_clone_is_alive() {
    let holly = Holly::spawn(EngineConfig::default());
    let survivor = holly.clone();
    let mut events = holly.subscribe();
    let mut inbound = holly.subscribe_inbound();

    // A live session task holds its own outbox sender; shutdown must end it too.
    let sid = SessionId::new("s");
    let turn = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    let done = collect_until_done(turn, &sid).await;
    assert!(matches!(done.last(), Some(OutEvent::Done { .. })));

    holly.shutdown().await;

    assert_closes(&mut events).await;
    assert_closes(&mut inbound).await;
    assert!(survivor.send(InMsg::prompt(sid, "again")).await.is_err());
    assert!(matches!(
        survivor.subscribe().recv().await,
        Err(RecvError::Closed)
    ));
    // Idempotent: a second shutdown from another clone returns at once.
    tokio::time::timeout(Duration::from_secs(1), survivor.shutdown())
        .await
        .expect("second shutdown should not block");
}
