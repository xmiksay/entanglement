//! `RecordSink` pluggable-persistence seam (#313). The persistence tap owns the
//! event-sourcing logic (route each record to its root, tombstone lag gaps) and
//! appends through a `RecordSink`; the default `FileSink` is the file store, and
//! an embedder can swap in any other target. This drives a real turn through a
//! *tee* sink — the file store plus an in-memory recorder — and asserts the
//! custom sink is handed the byte-for-byte identical record stream that lands on
//! disk, so an alternate sink inherits the tap unchanged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::persistence::{
    spawn_persistence_subscriber_with_sink, FileSink, RecordSink,
};
use entanglement_runtime::session_store::{read, LogPayload, LogRecord};

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
