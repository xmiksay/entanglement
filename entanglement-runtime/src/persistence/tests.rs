//! Tests for the persistence tap (split out under the 400-line cap).

use super::*;
use crate::session_store::{integrity_gap, SessionStore};
use entanglement_core::PersistenceSignal;
use std::sync::Mutex;
use tokio::sync::broadcast;

/// A [`RecordSink`] that captures every `(root, record)` it is handed, so a
/// test can assert an embedder sink sees the exact stream the file sink does.
#[derive(Default)]
struct RecordingSink {
    records: Mutex<Vec<(SessionId, LogRecord)>>,
    touched: Mutex<Vec<SessionId>>,
}

impl RecordSink for RecordingSink {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        self.records
            .lock()
            .unwrap()
            .push((root.clone(), record.clone()));
        Ok(())
    }

    fn touch(&self, root: &SessionId) {
        self.touched.lock().unwrap().push(root.clone());
    }
}

fn roots_map(pairs: &[(&str, &str)]) -> HashMap<SessionId, SessionId> {
    pairs
        .iter()
        .map(|(s, r)| (SessionId::new(*s), SessionId::new(*r)))
        .collect()
}

#[test]
fn record_gap_writes_tombstone_to_each_known_root() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let root_a = SessionId::new("root-a");
    let root_b = SessionId::new("root-b");
    for r in [&root_a, &root_b] {
        store
            .append(
                r,
                &LogRecord::new(r.clone(), LogPayload::In(InMsg::prompt(r.clone(), "hi"))),
            )
            .expect("append");
    }

    // A child routes into root-a; both roots also map to themselves.
    let roots = roots_map(&[
        ("child", "root-a"),
        ("root-a", "root-a"),
        ("root-b", "root-b"),
    ]);
    record_gap(&mut file_writer(&store), &roots, 7, "outbound");

    assert_eq!(integrity_gap(&store.read(&root_a).expect("read")), Some(7));
    assert_eq!(integrity_gap(&store.read(&root_b).expect("read")), Some(7));
}

#[test]
fn record_gap_dedups_sessions_sharing_a_root() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let root = SessionId::new("root");

    let roots = roots_map(&[("root", "root"), ("child1", "root"), ("child2", "root")]);
    record_gap(&mut file_writer(&store), &roots, 3, "inbound");

    let records = store.read(&root).expect("read");
    let gaps = records
        .iter()
        .filter(|r| matches!(r.payload, LogPayload::Gap { .. }))
        .count();
    assert_eq!(gaps, 1, "one tombstone despite three sessions on this root");
    assert_eq!(integrity_gap(&records), Some(3));
}

#[tokio::test]
async fn forced_lag_hands_a_custom_sink_the_same_gap_tombstones_as_the_file() {
    // Force a *real* broadcast lag to derive the dropped count the tap would
    // see, rather than hardcoding it: a capacity-2 channel overflowed by 5
    // sends drops 3 before the first recv.
    let (tx, mut rx) = tokio::sync::broadcast::channel::<u64>(2);
    for i in 0..5 {
        tx.send(i).expect("send");
    }
    let dropped = match rx.recv().await {
        Err(RecvError::Lagged(n)) => n,
        other => panic!("expected a forced lag, got {other:?}"),
    };

    let roots = roots_map(&[
        ("root-a", "root-a"),
        ("child", "root-a"),
        ("root-b", "root-b"),
    ]);

    // The file sink writes the tombstones to disk; the recording sink is the
    // stand-in embedder target. Both go through the identical lag code path.
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let mut file_sink = file_writer(&store);
    let recording = Arc::new(RecordingSink::default());
    let mut recording_writer = GapWriter::new(recording.clone(), PersistenceSignal::detached());

    record_gap(&mut file_sink, &roots, dropped, "outbound");
    record_gap(&mut recording_writer, &roots, dropped, "outbound");

    // The recording sink saw exactly one Gap per unique root, all carrying the
    // forced dropped count.
    let captured = recording.records.lock().unwrap();
    let mut captured_roots: Vec<String> = captured
        .iter()
        .map(|(root, record)| {
            assert!(
                matches!(record.payload, LogPayload::Gap { dropped: d } if d == dropped),
                "recording sink got a non-Gap record: {record:?}"
            );
            root.to_string()
        })
        .collect();
    captured_roots.sort();
    assert_eq!(captured_roots, vec!["root-a", "root-b"]);

    // And what the file sink persisted matches: resume refuses on both roots.
    for r in [SessionId::new("root-a"), SessionId::new("root-b")] {
        assert_eq!(integrity_gap(&store.read(&r).expect("read")), Some(dropped));
    }
}

#[test]
fn record_gap_with_no_known_roots_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(temp.path());
    let roots: HashMap<SessionId, SessionId> = HashMap::new();

    record_gap(&mut file_writer(&store), &roots, 5, "outbound");

    let dir = store.dir().expect("session_dir");
    let count = std::fs::read_dir(&dir).expect("read_dir").count();
    assert_eq!(count, 0, "no root known → no file written");
}

fn file_writer(store: &SessionStore) -> GapWriter {
    GapWriter::new(
        Arc::new(FileSink::new(store.clone())),
        PersistenceSignal::detached(),
    )
}

/// Fails the append calls whose 0-based index is in `fail`, storing the rest.
#[derive(Default)]
struct ScriptedSink {
    fail: Vec<usize>,
    calls: Mutex<usize>,
    stored: Mutex<Vec<LogRecord>>,
}

impl ScriptedSink {
    fn failing(fail: &[usize]) -> Arc<Self> {
        Arc::new(Self {
            fail: fail.to_vec(),
            ..Self::default()
        })
    }

    fn stored(&self) -> Vec<LogRecord> {
        self.stored.lock().unwrap().clone()
    }
}

impl RecordSink for ScriptedSink {
    fn append(&self, _root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        let mut calls = self.calls.lock().unwrap();
        let n = *calls;
        *calls += 1;
        if self.fail.contains(&n) {
            anyhow::bail!("scripted failure #{n}");
        }
        self.stored.lock().unwrap().push(record.clone());
        Ok(())
    }
}

fn prompt_record(label: &str) -> LogRecord {
    let sid = SessionId::new(label);
    LogRecord::new(sid.clone(), LogPayload::In(InMsg::prompt(sid, "hi")))
}

/// The labels (session ids) of the stored records, `gap:N` for a tombstone.
fn labels(records: &[LogRecord]) -> Vec<String> {
    records
        .iter()
        .map(|r| match r.payload {
            LogPayload::Gap { dropped } => format!("gap:{dropped}"),
            _ => r.session.to_string(),
        })
        .collect()
}

#[test]
fn failed_append_is_tombstoned_in_place() {
    let sink = ScriptedSink::failing(&[1]);
    let mut writer = GapWriter::new(sink.clone(), PersistenceSignal::detached());
    let root = SessionId::new("root");
    for label in ["a", "b", "c"] {
        writer.append(&root, &prompt_record(label), "test");
    }
    assert_eq!(labels(&sink.stored()), ["a", "gap:1", "c"]);
}

#[test]
fn failed_tombstone_lands_before_the_next_record() {
    // #1 is "b", #2 its tombstone; "c" then flushes the tombstone first.
    let sink = ScriptedSink::failing(&[1, 2]);
    let mut writer = GapWriter::new(sink.clone(), PersistenceSignal::detached());
    let root = SessionId::new("root");
    for label in ["a", "b", "c"] {
        writer.append(&root, &prompt_record(label), "test");
    }
    assert_eq!(labels(&sink.stored()), ["a", "gap:1", "c"]);
    let rows = sink.stored().into_iter().map(Ok::<_, ()>);
    let prefix = crate::session_store::resumable_prefix(rows);
    assert_eq!(labels(&prefix.records), ["a"]);
}

#[test]
fn records_behind_an_unwritten_tombstone_count_as_lost() {
    // "b" fails (#1), its tombstone fails (#2), "c"'s flush retry fails
    // (#3) so "c" joins the gap; "d" lands the 2-record tombstone first.
    let sink = ScriptedSink::failing(&[1, 2, 3]);
    let mut writer = GapWriter::new(sink.clone(), PersistenceSignal::detached());
    let root = SessionId::new("root");
    for label in ["a", "b", "c"] {
        writer.append(&root, &prompt_record(label), "test");
    }
    // One outage for the whole run of failures: logged once, not per record.
    assert_eq!(writer.outage.len(), 1);
    writer.append(&root, &prompt_record("d"), "test");
    assert!(writer.outage.is_empty(), "recovery ends the outage");
    assert_eq!(labels(&sink.stored()), ["a", "gap:2", "d"]);
}

#[test]
fn lag_gap_whose_tombstone_fails_is_written_before_the_next_record() {
    let sink = ScriptedSink::failing(&[1]);
    let mut writer = GapWriter::new(sink.clone(), PersistenceSignal::detached());
    let roots = roots_map(&[("root", "root")]);
    let root = SessionId::new("root");
    writer.append(&root, &prompt_record("a"), "test");
    record_gap(&mut writer, &roots, 4, "outbound");
    writer.append(&root, &prompt_record("b"), "test");
    assert_eq!(labels(&sink.stored()), ["a", "gap:4", "b"]);
}

#[test]
fn pending_gaps_are_flushed_on_shutdown_and_kept_per_root() {
    // #3 is the retry of r1's tombstone after r2's append, failing too.
    let sink = ScriptedSink::failing(&[0, 1, 3]);
    let mut writer = GapWriter::new(sink.clone(), PersistenceSignal::detached());
    let (r1, r2) = (SessionId::new("r1"), SessionId::new("r2"));
    writer.append(&r1, &prompt_record("a"), "test");
    // r1's pending gap does not hold back r2's stream.
    writer.append(&r2, &prompt_record("b"), "test");
    assert_eq!(labels(&sink.stored()), ["b"]);
    writer.flush_all();
    assert_eq!(labels(&sink.stored()), ["b", "gap:1"]);
}

/// Every buffered outage transition (#767) on `sub`, as `(root, kind, lost)`.
fn transitions(sub: &mut broadcast::Receiver<OutEvent>) -> Vec<(String, &'static str, u64)> {
    let mut out = Vec::new();
    while let Ok(ev) = sub.try_recv() {
        match ev {
            OutEvent::PersistenceDegraded { root, error } => {
                assert!(error.contains("scripted failure"), "{error}");
                out.push((root.to_string(), "degraded", 0));
            }
            OutEvent::PersistenceRecovered { root, lost } => {
                out.push((root.to_string(), "recovered", lost));
            }
            _ => {}
        }
    }
    out
}

/// A writer signalling into a live engine, plus a subscription to it (the
/// `Holly` is returned so it outlives the test body).
fn signalled_writer(sink: Arc<ScriptedSink>) -> (GapWriter, broadcast::Receiver<OutEvent>, Holly) {
    let holly = Holly::spawn(entanglement_core::EngineConfig::default());
    let sub = holly.subscribe();
    (GapWriter::new(sink, holly.persistence_signal()), sub, holly)
}

#[tokio::test]
async fn an_outage_signals_once_and_recovers_with_its_lost_count() {
    // "b" fails (#1), its tombstone (#2) and "c"'s flush retry (#3) too: one
    // outage, one degraded — then "d" lands the 2-record tombstone (#4).
    let (mut writer, mut sub, _holly) = signalled_writer(ScriptedSink::failing(&[1, 2, 3]));
    let root = SessionId::new("root");
    for label in ["a", "b", "c"] {
        writer.append(&root, &prompt_record(label), "test");
    }
    assert_eq!(transitions(&mut sub), [("root".into(), "degraded", 0)]);
    writer.append(&root, &prompt_record("d"), "test");
    writer.append(&root, &prompt_record("e"), "test");
    assert_eq!(transitions(&mut sub), [("root".into(), "recovered", 2)]);
}

#[tokio::test]
async fn outages_are_signalled_per_root_and_per_outage() {
    // Each failure's tombstone lands on the very next call, so every outage
    // is one record long; r1 fails twice — two separate outages.
    let (mut writer, mut sub, _holly) = signalled_writer(ScriptedSink::failing(&[0, 2, 6]));
    let (r1, r2) = (SessionId::new("r1"), SessionId::new("r2"));
    writer.append(&r1, &prompt_record("a"), "test"); // #0 fails, #1 gap
    writer.append(&r2, &prompt_record("b"), "test"); // #2 fails, #3 gap
    writer.append(&r1, &prompt_record("c"), "test"); // #4
    writer.append(&r2, &prompt_record("d"), "test"); // #5
    writer.append(&r1, &prompt_record("e"), "test"); // #6 fails, #7 gap
    assert_eq!(
        transitions(&mut sub),
        [
            ("r1".into(), "degraded", 0),
            ("r1".into(), "recovered", 1),
            ("r2".into(), "degraded", 0),
            ("r2".into(), "recovered", 1),
            ("r1".into(), "degraded", 0),
            ("r1".into(), "recovered", 1),
        ]
    );
}

#[tokio::test]
async fn a_healthy_sink_and_a_landed_lag_gap_signal_nothing() {
    let (mut writer, mut sub, _holly) = signalled_writer(ScriptedSink::failing(&[]));
    let root = SessionId::new("root");
    writer.append(&root, &prompt_record("a"), "test");
    // A lag is loss, but not an outage: its tombstone landed first try.
    record_gap(&mut writer, &roots_map(&[("root", "root")]), 3, "outbound");
    writer.append(&root, &prompt_record("b"), "test");
    assert!(transitions(&mut sub).is_empty());
}

#[tokio::test]
async fn a_stopped_roots_tombstone_lands_on_another_roots_append() {
    // r1's "b" (#1) and its tombstone (#2) fail, then r1 stops writing. r2's
    // append (#3) succeeds, so r1's tombstone is retried (#4) and recovers.
    let sink = ScriptedSink::failing(&[1, 2]);
    let (mut writer, mut sub, _holly) = signalled_writer(sink.clone());
    let (r1, r2) = (SessionId::new("r1"), SessionId::new("r2"));
    writer.append(&r1, &prompt_record("a"), "test");
    writer.append(&r1, &prompt_record("b"), "test");
    writer.append(&r2, &prompt_record("c"), "test");
    assert_eq!(
        transitions(&mut sub),
        [("r1".into(), "degraded", 0), ("r1".into(), "recovered", 1)]
    );
    assert_eq!(labels(&sink.stored()), ["a", "c", "gap:1"]);
    assert!(writer.outage.is_empty());
}

#[test]
fn the_default_public_error_drops_the_chain_and_control_chars_and_is_bounded() {
    let sink = ScriptedSink::failing(&[]);
    let e = anyhow::anyhow!("postgres://user:secret@db/x refused")
        .context("insert failed\nretry later");
    assert_eq!(sink.public_error(&e), "insert failed retry later");
    let long = anyhow::anyhow!("{}", "x".repeat(500));
    let cut = sink.public_error(&long);
    assert_eq!(cut.chars().count(), PUBLIC_ERROR_MAX);
    assert!(cut.ends_with('…'));
}

#[tokio::test]
async fn the_tap_never_logs_live_only_nested_call_events() {
    let holly = Holly::spawn(entanglement_core::EngineConfig::default());
    let sink = Arc::new(RecordingSink::default());
    let _tap = spawn_persistence_subscriber_with_sink(&holly, sink.clone());
    let sid = SessionId::new("s");
    holly.emit_live(OutEvent::NestedCall {
        session: sid.clone(),
        parent_request_id: "t1".into(),
        index: 1,
        tool: "read".into(),
        args: "a.txt".into(),
        duration_ms: 1,
        is_error: false,
    });
    holly.emit_live(OutEvent::NestedCallSummary {
        session: sid.clone(),
        parent_request_id: "t1".into(),
        suppressed: 1,
        errors: 0,
    });
    // A content event behind them: once it is logged, the two live events
    // ahead of it on the same channel have been seen (and skipped).
    holly.emit_for_session(&sid, |seq| OutEvent::TaskList {
        session: sid.clone(),
        seq,
        content: "fence".into(),
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while sink.records.lock().unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "fence never logged");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let records = sink.records.lock().unwrap();
    assert_eq!(records.len(), 1, "only the fence: {records:?}");
    assert!(matches!(
        &records[0].1.payload,
        LogPayload::Out(OutEvent::TaskList { .. })
    ));
}

/// ADR-0234: core announces the move before the successor's own start, so the
/// adoption pointer waits for that start to resolve the successor's root; the
/// move itself stays with the child's history.
#[tokio::test]
async fn a_move_announced_before_the_successor_starts_is_pointed_at_once_it_does() {
    let holly = Holly::spawn(entanglement_core::EngineConfig::default());
    let sink = Arc::new(RecordingSink::default());
    let _tap = spawn_persistence_subscriber_with_sink(&holly, sink.clone());
    let started = |s: &str, parent: Option<&str>| OutEvent::SessionStarted {
        session: SessionId::new(s),
        parent: parent.map(SessionId::new),
        predecessor: None,
        agent: "general".into(),
        model: None,
        root: parent.is_none(),
        ts: 0,
        user: None,
    };
    let emit = |ev: OutEvent| {
        let session = ev.session().cloned().expect("session event");
        holly.emit_for_session(&session, |_| ev);
    };
    emit(started("pred", None));
    emit(started("kid", Some("pred")));
    emit(OutEvent::ParentChanged {
        session: SessionId::new("kid"),
        parent: SessionId::new("succ"),
    });
    emit(started("succ", None));

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let records = loop {
        let records = sink.records.lock().unwrap().clone();
        if records
            .iter()
            .any(|(_, r)| matches!(r.payload, LogPayload::Adopted { .. }))
        {
            break records;
        }
        assert!(tokio::time::Instant::now() < deadline, "no pointer written");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let pos = |pred: &dyn Fn(&(SessionId, LogRecord)) -> bool| {
        records.iter().position(pred).expect("record present")
    };
    let moved = pos(&|(root, r)| {
        root.0 == "pred" && matches!(r.payload, LogPayload::Out(OutEvent::ParentChanged { .. }))
    });
    let succ_start = pos(&|(root, r)| {
        root.0 == "succ"
            && matches!(&r.payload, LogPayload::Out(OutEvent::SessionStarted { session, .. }) if session.0 == "succ")
    });
    let pointer = pos(&|(root, r)| {
        root.0 == "succ"
            && r.session.0 == "kid"
            && matches!(&r.payload, LogPayload::Adopted { from } if from.0 == "pred")
    });
    assert!(moved < succ_start && succ_start < pointer, "{records:?}");
    // The history's file gets a fresh retention clock with the pointer.
    assert_eq!(*sink.touched.lock().unwrap(), [SessionId::new("pred")]);
}
