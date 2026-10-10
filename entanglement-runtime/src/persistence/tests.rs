//! Tests for the persistence tap (split out under the 400-line cap).

use super::*;
use crate::session_store::{integrity_gap, SessionStore};
use std::sync::Mutex;

/// A [`RecordSink`] that captures every `(root, record)` it is handed, so a
/// test can assert an embedder sink sees the exact stream the file sink does.
#[derive(Default)]
struct RecordingSink {
    records: Mutex<Vec<(SessionId, LogRecord)>>,
}

impl RecordSink for RecordingSink {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        self.records
            .lock()
            .unwrap()
            .push((root.clone(), record.clone()));
        Ok(())
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
    let mut recording_writer = GapWriter::new(recording.clone());

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
    GapWriter::new(Arc::new(FileSink::new(store.clone())))
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
    let mut writer = GapWriter::new(sink.clone());
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
    let mut writer = GapWriter::new(sink.clone());
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
    let mut writer = GapWriter::new(sink.clone());
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
    let mut writer = GapWriter::new(sink.clone());
    let roots = roots_map(&[("root", "root")]);
    let root = SessionId::new("root");
    writer.append(&root, &prompt_record("a"), "test");
    record_gap(&mut writer, &roots, 4, "outbound");
    writer.append(&root, &prompt_record("b"), "test");
    assert_eq!(labels(&sink.stored()), ["a", "gap:4", "b"]);
}

#[test]
fn pending_gaps_are_flushed_on_shutdown_and_kept_per_root() {
    let sink = ScriptedSink::failing(&[0, 1]);
    let mut writer = GapWriter::new(sink.clone());
    let (r1, r2) = (SessionId::new("r1"), SessionId::new("r2"));
    writer.append(&r1, &prompt_record("a"), "test");
    // r1's pending gap must not leak into r2's stream.
    writer.append(&r2, &prompt_record("b"), "test");
    assert_eq!(labels(&sink.stored()), ["b"]);
    writer.flush_all();
    assert_eq!(labels(&sink.stored()), ["b", "gap:1"]);
}
