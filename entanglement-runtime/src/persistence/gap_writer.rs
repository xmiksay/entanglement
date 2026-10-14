//! The tap's write path: every [`RecordSink::append`] goes through
//! [`GapWriter`], which turns a failed append into a durable `Gap` (#723).

use super::RecordSink;
use crate::session_store::{LogPayload, LogRecord};
use entanglement_core::{PersistenceSignal, SessionId};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Wraps the sink so a dropped record never leaves a silent hole.
///
/// A failed append is a lost record, exactly like a broadcast lag, so it is
/// tombstoned as `Gap { dropped }` in that record's root. When the tombstone
/// itself can't be written (the store is still down), the count is held
/// per root and the tombstone is retried before that root's next record.
/// Until it lands, later records for the root are counted into the pending
/// gap rather than written: a record persisted after an unmarked hole would
/// sit inside what `resumable_prefix` treats as the intact prefix.
pub(super) struct GapWriter {
    sink: Arc<dyn RecordSink>,
    /// root → records lost but not yet tombstoned.
    pending: HashMap<SessionId, u64>,
    /// Roots whose sink is currently failing. WHY: a persistent outage would
    /// otherwise log an error per record; this logs its start and its end —
    /// and signals both to the heads (#767), so exactly these transitions go
    /// on the wire.
    pub(super) outage: HashSet<SessionId>,
    signal: PersistenceSignal,
}

impl GapWriter {
    pub(super) fn new(sink: Arc<dyn RecordSink>, signal: PersistenceSignal) -> Self {
        Self {
            sink,
            pending: HashMap::new(),
            outage: HashSet::new(),
            signal,
        }
    }

    /// Restart `root`'s retention clock ([`RecordSink::touch`]).
    pub(super) fn touch(&self, root: &SessionId) {
        self.sink.touch(root);
    }

    /// Appends `record` to `root`, first landing any pending gap there.
    /// `what` names the record for the error log.
    pub(super) fn append(&mut self, root: &SessionId, record: &LogRecord, what: &str) {
        if !self.flush(root) {
            self.add_pending(root, 1);
            return;
        }
        match self.sink.append(root, record) {
            Ok(()) => self.flush_others(root),
            Err(e) => {
                self.failed(root, what, &e);
                self.gap(root, 1);
            }
        }
    }

    /// The sink just took a record, so it may have healed: retry every
    /// *other* root's pending tombstone too. WHY: a root that ended or
    /// hibernated mid-outage writes no further record of its own, so its
    /// tombstone (and its `PersistenceRecovered`) would otherwise wait for
    /// tap shutdown. Bounded by the pending roots; free when none are.
    fn flush_others(&mut self, root: &SessionId) {
        if self.pending.is_empty() {
            return;
        }
        let others: Vec<SessionId> = self
            .pending
            .keys()
            .filter(|r| *r != root)
            .cloned()
            .collect();
        for other in others {
            self.flush(&other);
        }
    }

    /// Records `dropped` lost records in `root` (a lag or a failed append).
    pub(super) fn gap(&mut self, root: &SessionId, dropped: u64) {
        self.add_pending(root, dropped);
        self.flush(root);
    }

    /// Last-chance retry of every pending tombstone, on tap shutdown.
    pub(super) fn flush_all(&mut self) {
        let roots: Vec<SessionId> = self.pending.keys().cloned().collect();
        for root in roots {
            if !self.flush(&root) {
                let dropped = self.pending.get(&root).copied().unwrap_or_default();
                tracing::error!(
                    "Persistence for {root} ends with {dropped} lost record(s) and no gap \
                     tombstone: the sink never recovered"
                );
            }
        }
    }

    /// Logs a sink failure: an error when it starts an outage for `root`,
    /// debug while the outage lasts.
    fn failed(&mut self, root: &SessionId, what: &str, e: &anyhow::Error) {
        if self.outage.insert(root.clone()) {
            tracing::error!(
                "Failed to persist {what} for {root}: {e:#}; recording a gap and holding \
                 this root's records until the sink recovers"
            );
            // The full chain stays in the log above; the wire gets the sink's
            // public summary (it is broadcast to every connected head).
            self.signal.degraded(root, self.sink.public_error(e));
        } else {
            tracing::debug!("Persistence for {root} still failing ({what}): {e:#}");
        }
    }

    fn add_pending(&mut self, root: &SessionId, n: u64) {
        let slot = self.pending.entry(root.clone()).or_default();
        *slot = slot.saturating_add(n);
    }

    /// Writes `root`'s pending tombstone, if any. `true` when nothing is
    /// pending any more.
    fn flush(&mut self, root: &SessionId) -> bool {
        let Some(&dropped) = self.pending.get(root) else {
            return true;
        };
        let tombstone = LogRecord::new(root.clone(), LogPayload::Gap { dropped });
        match self.sink.append(root, &tombstone) {
            Ok(()) => {
                self.pending.remove(root);
                if self.outage.remove(root) {
                    tracing::warn!(
                        "Persistence for {root} recovered; recorded a gap of {dropped} lost record(s)"
                    );
                    self.signal.recovered(root, dropped);
                }
                true
            }
            Err(e) => {
                self.failed(root, "gap tombstone", &e);
                false
            }
        }
    }
}
