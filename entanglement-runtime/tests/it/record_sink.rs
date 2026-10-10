//! `RecordSink` pluggable-persistence seam (#313). The persistence tap owns the
//! event-sourcing logic (route each record to its root, tombstone lag gaps) and
//! appends through a `RecordSink`; the default `FileSink` is the file store, and
//! an embedder can swap in any other target. This drives a real turn through a
//! *tee* sink — the file store plus an in-memory recorder — and asserts the
//! custom sink is handed the byte-for-byte identical record stream that lands on
//! disk, so an alternate sink inherits the tap unchanged. A sink whose
//! `append` fails gets the lost record tombstoned as a `Gap` (#723).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::persistence::{
    spawn_persistence_subscriber_with_sink, FileSink, RecordSink,
};
use entanglement_runtime::session_store::{
    check_for_resume, read, resumable_prefix, LogPayload, LogRecord, Truncation,
};

/// Forwards every record to the real file store *and* captures a clone, so the
/// test can compare what an embedder sink sees against what hits the disk.
struct TeeSink {
    file: FileSink,
    captured: Arc<Mutex<Vec<LogRecord>>>,
}

impl RecordSink for TeeSink {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        self.captured.lock().unwrap().push(record.clone());
        self.file.append(root, record)
    }
}

#[tokio::test]
async fn custom_sink_receives_the_same_record_stream_as_the_file() {
    // A distinct cwd keys a distinct log file (session_store hashes the cwd).
    let tmp = tempfile::tempdir().expect("temp dir");
    let cwd = tmp.path().to_path_buf();
    let sid = SessionId::new("sink-root");

    let captured = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(EngineConfig::default());
    let sink = Arc::new(TeeSink {
        file: FileSink::new(cwd.clone()),
        captured: captured.clone(),
    });
    let _tap = spawn_persistence_subscriber_with_sink(&holly, sink);

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .expect("send prompt");

    // Drain our own subscription to the turn's Done.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("timed out waiting for Done")
            .expect("broadcast closed");
        if ev.session() == Some(&sid) && matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }

    // The tap writes concurrently; wait until it has flushed the turn's Done to
    // the sink before comparing (both the file write and the capture happen in
    // the same `append`, so seeing Done in the capture means the file has it).
    let flushed = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let has_done = captured.lock().unwrap().iter().any(|r| {
            matches!(&r.payload, LogPayload::Out(OutEvent::Done { session, .. }) if *session == sid)
        });
        if has_done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < flushed,
            "tap never flushed the turn's Done to the sink"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The custom sink saw the identical stream the file persisted, in order.
    let on_disk = read(&cwd, &sid).expect("read log");
    let seen = captured.lock().unwrap().clone();
    let seen_json: Vec<String> = seen
        .iter()
        .map(|r| serde_json::to_string(r).expect("serialize"))
        .collect();
    let disk_json: Vec<String> = on_disk
        .iter()
        .map(|r| serde_json::to_string(r).expect("serialize"))
        .collect();
    assert_eq!(
        seen_json, disk_json,
        "the custom sink must receive the exact record stream the file sink writes"
    );

    // Sanity: the stream is non-trivial and carries the inbound prompt plus the
    // outbound turn — not an empty or file-only artefact.
    assert!(
        seen.iter()
            .any(|r| matches!(&r.payload, LogPayload::In(InMsg::Prompt { .. }))),
        "the inbound prompt must reach the sink"
    );
    assert!(
        seen.iter()
            .any(|r| matches!(&r.payload, LogPayload::Out(OutEvent::SessionStarted { .. }))),
        "the session lifecycle must reach the sink"
    );
}

/// An embedder-style sink whose `append` fails on scripted call indices (a
/// full backlog, a failed insert) and stores everything else in memory.
struct FlakySink {
    fail: Vec<usize>,
    calls: Mutex<usize>,
    stored: Arc<Mutex<Vec<LogRecord>>>,
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

/// Drives one echo turn through a [`FlakySink`] failing `fail`, returning
/// what it stored once the turn's `Done` landed.
async fn turn_through_flaky_sink(sid: &SessionId, fail: &[usize]) -> Vec<LogRecord> {
    let stored = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(EngineConfig::default());
    let sink = Arc::new(FlakySink {
        fail: fail.to_vec(),
        calls: Mutex::new(0),
        stored: stored.clone(),
    });
    let _tap = spawn_persistence_subscriber_with_sink(&holly, sink);
    holly
        .send(InMsg::prompt(sid.clone(), "hello"))
        .await
        .expect("send prompt");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let done = stored.lock().unwrap().iter().any(|r| {
            matches!(&r.payload, LogPayload::Out(OutEvent::Done { session, .. }) if session == sid)
        });
        if done {
            return stored.lock().unwrap().clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn's Done never reached the sink"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// #723: a record the sink refuses is a hole, so the tap tombstones it —
/// the in-tree resume refuses the log and the embedder helper cuts there.
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
    assert!(check_for_resume(&stored, |_| true).is_err());
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
    assert!(check_for_resume(&stored, |_| true).is_err());
}
