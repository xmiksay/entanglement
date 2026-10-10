use crate::session_store::{self, LogPayload, LogRecord};
use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

mod gap_writer;
use gap_writer::GapWriter;

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
///
/// **Gap ownership** (#723): an `Err` from `append` means "this record is
/// lost", and the tap — not the sink — marks it: it appends a
/// [`LogPayload::Gap`] tombstone to the same root, and if that fails too it
/// retries the tombstone before the root's next record (holding that root's
/// records back as further loss until it lands). A broadcast lag is
/// tombstoned the same way. So a sink must either persist a record or
/// return `Err`; one that drops a record *after* returning `Ok` (e.g. its
/// writer task's insert fails later) has hidden the loss from the tap and
/// must write its own `Gap` row for that root, or a resume replays a hole.
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

/// Records a [`LogPayload::Gap`] tombstone into every known root session
/// (via the [`GapWriter`], so a tombstone the sink refuses stays pending).
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
    writer: &mut GapWriter,
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
        writer.gap(root, dropped);
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
/// `InMsg::Resume` is skipped (it carries the whole prior log → recursion/bloat).
/// `InMsg::Spawn` itself is never appended verbatim — persisting it the moment it
/// arrives would create a stray single-line child file (`roots` can't resolve the
/// child to its parent's root until the child's `SessionStarted` shows up).
/// Instead its `prompt` is cached (`pending_spawn_prompts`) and, once that
/// `SessionStarted` is observed and `roots` correctly resolves the child, a
/// synthesized `InMsg::Prompt { session: child, .. }` record is appended right
/// after it (#421) — so replay reconstructs the user-role instruction that
/// framed the child's first turn, not just the assistant's eventual reply. The
/// cache entry is consumed on first use, so a resumed child's re-announced
/// `SessionStarted` (no matching cache entry — resume never re-sends `Spawn`)
/// never re-synthesizes or duplicates the record. The supervisor-global queries
/// `InMsg::ListSessions` / `InMsg::ReplayFrom` and their session-less replies
/// `OutEvent::SessionList` / `OutEvent::History` carry no durable state, so they
/// are skipped too (#160).
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
/// tombstoning lost records (broadcast lag *and* a failed `append`) as `Gap`s —
/// lives here, so every sink inherits it. The
/// file-backed path is just this function with a [`FileSink`].
pub fn spawn_persistence_subscriber_with_sink(
    holly: &Holly,
    sink: Arc<dyn RecordSink>,
) -> tokio::task::JoinHandle<()> {
    let mut events = holly.subscribe();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        let mut writer = GapWriter::new(sink);
        // session id → its root session id.
        let mut roots: HashMap<SessionId, SessionId> = HashMap::new();
        // child session id → its spawn-initiating prompt text, cached from
        // `InMsg::Spawn` until the child's `SessionStarted` resolves its root
        // (#421); consumed (removed) the moment it's synthesized into a record.
        let mut pending_spawn_prompts: HashMap<SessionId, String> = HashMap::new();
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
                        // A `Spawn`'s prompt is cached, not appended directly —
                        // `roots` can't resolve the child until its
                        // `SessionStarted` arrives (see the doc comment above).
                        if let InMsg::Spawn { session: child, prompt, .. } = &msg {
                            pending_spawn_prompts.insert(child.clone(), prompt.clone());
                            continue;
                        }
                        // Resume is skipped (recursion/bloat); the
                        // supervisor-global queries carry no durable state, so
                        // ListSessions/ReplayFrom are skipped too (#160).
                        if matches!(
                            msg,
                            InMsg::Resume { .. }
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
                        writer.append(&root, &record, "inbound message");
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} inbound messages");
                        record_gap(&mut writer, &roots, n, "inbound");
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
                        let record = LogRecord::new(session_id.clone(), LogPayload::Out(ev));
                        writer.append(&root, &record, "outbound event");
                        // The child's root is now resolvable — synthesize its
                        // spawn-initiating prompt right after `SessionStarted` so
                        // it pairs (via `pair_records`) with the child's own next
                        // event, not `SessionStarted` itself (#421).
                        if let Some(prompt) = pending_spawn_prompts.remove(&session_id) {
                            let synthesized = InMsg::prompt(session_id.clone(), prompt);
                            let record = LogRecord::new(session_id, LogPayload::In(synthesized));
                            writer.append(&root, &record, "synthesized spawn prompt");
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} outbound events");
                        record_gap(&mut writer, &roots, n, "outbound");
                    }
                    Err(RecvError::Closed) => out_open = false,
                },
            }
        }
        writer.flush_all();
    })
}

#[cfg(test)]
mod tests;
