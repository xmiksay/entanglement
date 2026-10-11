//! Session-log outage signal (#767): a real turn through the persistence tap
//! with a failing `RecordSink` surfaces exactly one `PersistenceDegraded` and
//! one `PersistenceRecovered` (carrying the lost count) on the wire, nothing
//! for a healthy sink, and never writes either into the sink itself.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::persistence::spawn_persistence_subscriber_with_sink;
use entanglement_runtime::session_store::{LogPayload, LogRecord};
use tokio::sync::broadcast::Receiver;

use crate::record_sink::{wait_for_done, FlakySink};

/// Runs one echo turn through a [`FlakySink`] failing `fail`, returning the
/// stored records and every outage transition broadcast meanwhile.
async fn turn(sid: &SessionId, fail: &[usize]) -> (Vec<LogRecord>, Vec<OutEvent>) {
    let stored = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let _tap = spawn_persistence_subscriber_with_sink(&holly, FlakySink::new(fail, stored.clone()));
    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .expect("send prompt");
    let records = wait_for_done(&stored, sid, 0).await;
    // The tap emits a transition before it appends the record that caused
    // it, so once the Done is stored every transition is already buffered.
    (records, transitions(&mut sub))
}

fn transitions(sub: &mut Receiver<OutEvent>) -> Vec<OutEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = sub.try_recv() {
        if matches!(
            ev,
            OutEvent::PersistenceDegraded { .. } | OutEvent::PersistenceRecovered { .. }
        ) {
            out.push(ev);
        }
    }
    out
}

fn is_outage_record(record: &LogRecord) -> bool {
    matches!(
        record.payload,
        LogPayload::Out(
            OutEvent::PersistenceDegraded { .. } | OutEvent::PersistenceRecovered { .. }
        )
    )
}

#[tokio::test]
async fn a_failing_sink_signals_one_outage_and_its_recovery() {
    let sid = SessionId::new("outage-root");
    // Record #2 fails, its tombstone (#3) and the next flush retry (#4) too:
    // one outage holding two records, recovered by the tombstone at #5.
    let (records, events) = turn(&sid, &[2, 3, 4]).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(
        matches!(&events[0], OutEvent::PersistenceDegraded { root, error }
            if *root == sid && error.contains("backlog full")),
        "{:?}",
        events[0]
    );
    assert_eq!(
        events[1],
        OutEvent::PersistenceRecovered {
            root: sid.clone(),
            lost: 2
        }
    );
    assert!(matches!(records[2].payload, LogPayload::Gap { dropped: 2 }));
    // No feedback loop: the tap never tries to persist its own signal.
    assert!(!records.iter().any(is_outage_record));
}

#[tokio::test]
async fn a_healthy_sink_signals_nothing() {
    let sid = SessionId::new("healthy-root");
    let (records, events) = turn(&sid, &[]).await;
    assert!(events.is_empty(), "{events:?}");
    assert!(!records
        .iter()
        .any(|r| matches!(r.payload, LogPayload::Gap { .. })));
}

#[tokio::test]
async fn an_unrecovered_outage_still_lets_the_tap_finish_at_shutdown() {
    // Every append fails: the outage never ends, and the tap — holding only
    // a weak signal, not a `Holly` — must still drain and exit on shutdown.
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let stored = Arc::new(Mutex::new(Vec::new()));
    let all: Vec<usize> = (0..10_000).collect();
    let tap = spawn_persistence_subscriber_with_sink(&holly, FlakySink::new(&all, stored.clone()));
    let sid = SessionId::new("dead-root");
    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .expect("send prompt");
    // The tap runs behind the turn, so wait for both the Done and the signal.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (mut degraded, mut done) = (0, false);
    while degraded == 0 || !done {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("timed out waiting for Done and the degraded signal")
            .expect("broadcast open");
        degraded += usize::from(matches!(ev, OutEvent::PersistenceDegraded { .. }));
        done |= ev.session() == Some(&sid) && matches!(ev, OutEvent::Done { .. });
    }
    drop(holly);
    tokio::time::timeout(Duration::from_secs(5), tap)
        .await
        .expect("the tap finished after the engine went away")
        .expect("tap task");
    // Whatever is still buffered: no second degraded, no recovery.
    while let Ok(ev) = sub.recv().await {
        degraded += usize::from(matches!(ev, OutEvent::PersistenceDegraded { .. }));
        assert!(!matches!(ev, OutEvent::PersistenceRecovered { .. }));
    }
    assert_eq!(degraded, 1, "one signal for the whole outage");
    assert!(stored.lock().unwrap().is_empty());
}
