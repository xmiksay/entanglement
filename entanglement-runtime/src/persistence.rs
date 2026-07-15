use crate::session_store::{self, LogPayload, LogRecord};
use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// A pluggable append target for the persistence tap.
///
/// The tap ([`spawn_persistence_subscriber_with_sink`]) owns the event-sourcing
/// logic — routing every record to its root session, tombstoning broadcast-lag
/// gaps — and hands each finished [`LogRecord`] to a sink. An embedder that
/// persists elsewhere (e.g. a Postgres `assistant_events` table) implements this
/// one method and inherits the tap's gap/lag handling instead of forking the
/// whole subscriber.
///
/// `append` is synchronous: the default [`FileSink`] writes one `writeln!`. A
/// sink whose backing store can block (network, DB) must **not** block the tap —
/// that starves the broadcast receiver and manufactures the very `Gap`
/// tombstones this trait exists to avoid. Such a sink should put a bounded
/// channel + dedicated writer task behind `append` and return immediately (drop
/// past the bound, surfacing back-pressure as an error, rather than await).
pub trait RecordSink: Send + Sync {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()>;
}

/// The default sink: appends to `{root_id}.jsonl` under the `session_store`
/// layout rooted at `cwd`, preserving the original file-backed behavior.
pub struct FileSink {
    cwd: PathBuf,
}

impl FileSink {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl RecordSink for FileSink {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        session_store::append(&self.cwd, root, record)
    }
}

/// Records a [`LogPayload::Gap`] tombstone into every known root session.
///
/// The persistence tap reads Holly's lossy broadcast; a fast turn (long
/// streamed answer + many tool round-trips) can outrun the sink and drop a
/// contiguous run of events (`RecvError::Lagged`). The log stays well-formed,
/// so replay would otherwise fold an incomplete history into a wrong `Context`
/// with no error surfaced (#104). A tombstone poisons the affected roots so
/// [`crate::session_store::integrity_gap`] makes resume refuse.
///
/// A lag gives no signal *which* session lost records, so every known root is
/// marked — conservatively correct: the data is genuinely gone and cannot be
/// attributed. Before any `SessionStarted` is seen there is no root to mark and
/// nothing yet resumable, so the drop is only warned.
fn record_gap(
    sink: &dyn RecordSink,
    roots: &HashMap<SessionId, SessionId>,
    dropped: u64,
    kind: &str,
) {
    let unique: HashSet<&SessionId> = roots.values().collect();
    if unique.is_empty() {
        tracing::warn!(
            "Persistence lagged ({dropped} {kind} records) before any session root was known; \
             no tombstone written"
        );
        return;
    }
    for root in unique {
        let record = LogRecord::new(root.clone(), LogPayload::Gap { dropped });
        if let Err(e) = sink.append(root, &record) {
            tracing::error!("Failed to persist {kind} gap tombstone for {root}: {e}");
        }
    }
}

/// Spawns a persistence subscriber that logs both inbound `InMsg` and outbound
/// `OutEvent` to disk, making each root session a self-contained resumable file.
///
/// It subscribes to the outbound event stream *and* the inbound message fan-out
/// (`holly.subscribe_inbound()`), so replay reconstructs user turns from the
/// logged `InMsg::Prompt` records — without them `Session::replay` folds zero
/// user messages and a resumed session appears to forget the conversation.
///
/// `InMsg::Resume` is skipped (it carries the whole prior log → recursion/bloat)
/// and `InMsg::Spawn` is skipped (it would create a stray single-line child file
/// that lists as a bogus root; a child's turns are already captured in the root
/// file via out events). The supervisor-global queries `InMsg::ListSessions` /
/// `InMsg::ReplayFrom` and their session-less replies `OutEvent::SessionList` /
/// `OutEvent::History` carry no durable state, so they are skipped too (#160).
///
/// A `roots` map, folded from `SessionStarted { root, parent }`, routes every
/// record (root + spawned children) into the **root's** `{root_id}.jsonl`. Before
/// `SessionStarted` is seen the message's own session id is used as a fallback —
/// the first prompt arrives before its `SessionStarted`, but for a root session
/// the id is identical so the record lands in the same file either way.
pub fn spawn_persistence_subscriber(holly: &Holly, cwd: PathBuf) -> tokio::task::JoinHandle<()> {
    spawn_persistence_subscriber_with_sink(holly, Arc::new(FileSink::new(cwd)))
}

/// Same tap as [`spawn_persistence_subscriber`], but appends every record
/// through a caller-supplied [`RecordSink`] instead of the default file store.
///
/// All the event-sourcing logic — routing each record to its root session and
/// tombstoning broadcast-lag `Gap`s — lives here, so every sink inherits it. The
/// file-backed path is just this function with a [`FileSink`].
pub fn spawn_persistence_subscriber_with_sink(
    holly: &Holly,
    sink: Arc<dyn RecordSink>,
) -> tokio::task::JoinHandle<()> {
    let mut events = holly.subscribe();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        // session id → its root session id.
        let mut roots: HashMap<SessionId, SessionId> = HashMap::new();
        // Each side closes independently on shutdown (the supervisor drops the
        // inbound sender when it exits, then sessions drop the outbound senders).
        // Keep draining the still-open side so buffered events aren't lost — a
        // one-shot `run` shuts the engine down the instant the turn ends.
        let mut in_open = true;
        let mut out_open = true;

        while in_open || out_open {
            // Bias toward inbound: a `Prompt` is causally before the outbound
            // events it produces, so draining inbound first keeps `In` records
            // ahead of their paired `Out` records on disk (record pairing on
            // replay assumes that order).
            tokio::select! {
                biased;

                in_msg = inbound.recv(), if in_open => match in_msg {
                    Ok(msg) => {
                        // Resume/Spawn are skipped (recursion/bloat / stray child
                        // file); the supervisor-global queries carry no durable
                        // state, so ListSessions/ReplayFrom are skipped too (#160).
                        if matches!(
                            msg,
                            InMsg::Resume { .. }
                                | InMsg::Spawn { .. }
                                | InMsg::ListSessions { .. }
                                | InMsg::ReplayFrom { .. }
                        ) {
                            continue;
                        }
                        let Some(session_id) = msg.session().cloned() else {
                            continue;
                        };
                        let root = roots
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_else(|| session_id.clone());
                        let record = LogRecord::new(session_id, LogPayload::In(msg));
                        if let Err(e) = sink.append(&root, &record) {
                            tracing::error!("Failed to persist inbound message: {}", e);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} inbound messages");
                        record_gap(sink.as_ref(), &roots, n, "inbound");
                    }
                    Err(RecvError::Closed) => in_open = false,
                },

                out = events.recv(), if out_open => match out {
                    Ok(ev) => {
                        // A session-less query reply (SessionList/History, #160)
                        // is transient — never persisted.
                        let Some(session_id) = ev.session().cloned() else {
                            continue;
                        };
                        if let OutEvent::SessionStarted { session, parent, root, .. } = &ev {
                            let root_id = if *root {
                                session.clone()
                            } else {
                                parent
                                    .as_ref()
                                    .and_then(|p| roots.get(p).cloned())
                                    .unwrap_or_else(|| session.clone())
                            };
                            roots.insert(session.clone(), root_id);
                        }
                        let root = roots
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_else(|| session_id.clone());
                        let record = LogRecord::new(session_id, LogPayload::Out(ev));
                        if let Err(e) = sink.append(&root, &record) {
                            tracing::error!("Failed to persist outbound event: {}", e);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} outbound events");
                        record_gap(sink.as_ref(), &roots, n, "outbound");
                    }
                    Err(RecvError::Closed) => out_open = false,
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{append, integrity_gap, read, session_dir};
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
        let cwd = temp.path();
        let root_a = SessionId::new("root-a");
        let root_b = SessionId::new("root-b");
        for r in [&root_a, &root_b] {
            append(
                cwd,
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
        record_gap(&FileSink::new(cwd.to_path_buf()), &roots, 7, "outbound");

        assert_eq!(integrity_gap(&read(cwd, &root_a).expect("read")), Some(7));
        assert_eq!(integrity_gap(&read(cwd, &root_b).expect("read")), Some(7));
    }

    #[test]
    fn record_gap_dedups_sessions_sharing_a_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path();
        let root = SessionId::new("root");

        let roots = roots_map(&[("root", "root"), ("child1", "root"), ("child2", "root")]);
        record_gap(&FileSink::new(cwd.to_path_buf()), &roots, 3, "inbound");

        let records = read(cwd, &root).expect("read");
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
        let cwd = temp.path();
        let file_sink = FileSink::new(cwd.to_path_buf());
        let recording = RecordingSink::default();

        record_gap(&file_sink, &roots, dropped, "outbound");
        record_gap(&recording, &roots, dropped, "outbound");

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
            assert_eq!(integrity_gap(&read(cwd, &r).expect("read")), Some(dropped));
        }
    }

    #[test]
    fn record_gap_with_no_known_roots_writes_nothing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path();
        let roots: HashMap<SessionId, SessionId> = HashMap::new();

        record_gap(&FileSink::new(cwd.to_path_buf()), &roots, 5, "outbound");

        let dir = session_dir(cwd).expect("session_dir");
        let count = std::fs::read_dir(&dir).expect("read_dir").count();
        assert_eq!(count, 0, "no root known → no file written");
    }
}
