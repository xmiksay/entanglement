use crate::session_store::{append, LogPayload, LogRecord};
use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::sync::broadcast::error::RecvError;

/// Records a [`LogPayload::Gap`] tombstone into every known root session file.
///
/// The persistence tap reads Holly's lossy broadcast; a fast turn (long
/// streamed answer + many tool round-trips) can outrun disk appends and drop a
/// contiguous run of events (`RecvError::Lagged`). The file stays well-formed,
/// so replay would otherwise fold an incomplete history into a wrong `Context`
/// with no error surfaced (#104). A tombstone poisons the affected logs so
/// [`crate::session_store::integrity_gap`] makes resume refuse.
///
/// A lag gives no signal *which* session lost records, so every known root is
/// marked — conservatively correct: the data is genuinely gone and cannot be
/// attributed. Before any `SessionStarted` is seen there is no root to mark and
/// nothing yet resumable, so the drop is only warned.
fn record_gap(cwd: &Path, roots: &HashMap<SessionId, SessionId>, dropped: u64, kind: &str) {
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
        if let Err(e) = append(cwd, root, &record) {
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
/// file via out events).
///
/// A `roots` map, folded from `SessionStarted { root, parent }`, routes every
/// record (root + spawned children) into the **root's** `{root_id}.jsonl`. Before
/// `SessionStarted` is seen the message's own session id is used as a fallback —
/// the first prompt arrives before its `SessionStarted`, but for a root session
/// the id is identical so the record lands in the same file either way.
pub fn spawn_persistence_subscriber(holly: &Holly, cwd: PathBuf) -> tokio::task::JoinHandle<()> {
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
                        if matches!(msg, InMsg::Resume { .. } | InMsg::Spawn { .. }) {
                            continue;
                        }
                        let session_id = msg.session().clone();
                        let root = roots
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_else(|| session_id.clone());
                        let record = LogRecord::new(session_id, LogPayload::In(msg));
                        if let Err(e) = append(&cwd, &root, &record) {
                            tracing::error!("Failed to persist inbound message: {}", e);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} inbound messages");
                        record_gap(&cwd, &roots, n, "inbound");
                    }
                    Err(RecvError::Closed) => in_open = false,
                },

                out = events.recv(), if out_open => match out {
                    Ok(ev) => {
                        let session_id = ev.session().clone();
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
                        if let Err(e) = append(&cwd, &root, &record) {
                            tracing::error!("Failed to persist outbound event: {}", e);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} outbound events");
                        record_gap(&cwd, &roots, n, "outbound");
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
                &LogRecord::new(
                    r.clone(),
                    LogPayload::In(InMsg::Prompt {
                        session: r.clone(),
                        text: "hi".into(),
                    }),
                ),
            )
            .expect("append");
        }

        // A child routes into root-a; both roots also map to themselves.
        let roots = roots_map(&[
            ("child", "root-a"),
            ("root-a", "root-a"),
            ("root-b", "root-b"),
        ]);
        record_gap(cwd, &roots, 7, "outbound");

        assert_eq!(integrity_gap(&read(cwd, &root_a).expect("read")), Some(7));
        assert_eq!(integrity_gap(&read(cwd, &root_b).expect("read")), Some(7));
    }

    #[test]
    fn record_gap_dedups_sessions_sharing_a_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path();
        let root = SessionId::new("root");

        let roots = roots_map(&[("root", "root"), ("child1", "root"), ("child2", "root")]);
        record_gap(cwd, &roots, 3, "inbound");

        let records = read(cwd, &root).expect("read");
        let gaps = records
            .iter()
            .filter(|r| matches!(r.payload, LogPayload::Gap { .. }))
            .count();
        assert_eq!(gaps, 1, "one tombstone despite three sessions on this root");
        assert_eq!(integrity_gap(&records), Some(3));
    }

    #[test]
    fn record_gap_with_no_known_roots_writes_nothing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cwd = temp.path();
        let roots: HashMap<SessionId, SessionId> = HashMap::new();

        record_gap(cwd, &roots, 5, "outbound");

        let dir = session_dir(cwd).expect("session_dir");
        let count = std::fs::read_dir(&dir).expect("read_dir").count();
        assert_eq!(count, 0, "no root known → no file written");
    }
}
