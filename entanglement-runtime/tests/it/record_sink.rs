//! The `RecordSink` seam's loss handling (#313, #723): a sink whose `append`
//! fails gets the lost record tombstoned as a `Gap`, which resume refuses and
//! `resumable_prefix` cuts at.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::persistence::{spawn_persistence_subscriber_with_sink, RecordSink};
use entanglement_runtime::session_store::{
    integrity_gap, resumable_prefix, LogPayload, LogRecord, Truncation,
};

/// An embedder-style sink whose `append` fails on scripted call indices (a
/// full backlog, a failed insert) and stores everything else in memory.
pub(crate) struct FlakySink {
    fail: Vec<usize>,
    calls: Mutex<usize>,
    stored: Arc<Mutex<Vec<LogRecord>>>,
}

impl FlakySink {
    pub(crate) fn new(fail: &[usize], stored: Arc<Mutex<Vec<LogRecord>>>) -> Arc<Self> {
        Arc::new(Self {
            fail: fail.to_vec(),
            calls: Mutex::new(0),
            stored,
        })
    }
}

impl RecordSink for FlakySink {
    fn append(&self, _root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        let mut calls = self.calls.lock().unwrap();
        let n = *calls;
        *calls += 1;
        if self.fail.contains(&n) {
            anyhow::bail!("backlog full");
        }
        self.stored.lock().unwrap().push(record.clone());
        Ok(())
    }
}

/// Waits until `stored` holds a `Done` for `sid` at index `from` or later,
/// returning a snapshot of it.
pub(crate) async fn wait_for_done(
    stored: &Mutex<Vec<LogRecord>>,
    sid: &SessionId,
    from: usize,
) -> Vec<LogRecord> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = stored.lock().unwrap().clone();
        let done = snapshot.iter().skip(from).any(|r| {
            matches!(&r.payload, LogPayload::Out(OutEvent::Done { session, .. }) if session == sid)
        });
        if done {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn's Done never reached the sink"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drives one echo turn through a [`FlakySink`] failing `fail`, returning
/// what it stored once the turn's `Done` landed.
pub(crate) async fn turn_through_flaky_sink(sid: &SessionId, fail: &[usize]) -> Vec<LogRecord> {
    let stored = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(EngineConfig::default());
    let _tap = spawn_persistence_subscriber_with_sink(&holly, FlakySink::new(fail, stored.clone()));
    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .expect("send prompt");
    wait_for_done(&stored, sid, 0).await
}

/// #723: a record the sink refuses is a hole, so the tap tombstones it —
/// resume refuses the log and the embedder helper cuts there.
#[tokio::test]
async fn sink_append_error_records_a_gap_resume_detects() {
    let sid = SessionId::new("flaky-root");
    // Call #2 is the turn's third record; call #3 is then its tombstone.
    let stored = turn_through_flaky_sink(&sid, &[2]).await;
    assert!(
        matches!(stored[2].payload, LogPayload::Gap { dropped: 1 }),
        "the lost record is tombstoned in place: {:?}",
        stored[2].payload
    );
    assert!(integrity_gap(&stored).is_some());
    let prefix = resumable_prefix(stored.into_iter().map(Ok::<_, ()>));
    assert_eq!(prefix.records.len(), 2);
    assert_eq!(
        prefix.truncation,
        Some(Truncation::Gap {
            index: 2,
            dropped: 1
        })
    );
}

/// #723: when the tombstone write fails too, it is written before the
/// root's next successful record — the hole is never silently skipped.
#[tokio::test]
async fn failed_gap_write_lands_before_the_next_record() {
    let sid = SessionId::new("flaky-root-2");
    let stored = turn_through_flaky_sink(&sid, &[2, 3]).await;
    assert!(
        matches!(stored[2].payload, LogPayload::Gap { dropped: 1 }),
        "the retried tombstone precedes the next record: {:?}",
        stored[2].payload
    );
    assert_eq!(
        stored
            .iter()
            .filter(|r| matches!(r.payload, LogPayload::Gap { .. }))
            .count(),
        1
    );
    assert!(integrity_gap(&stored).is_some());
}
