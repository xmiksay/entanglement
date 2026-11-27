//! The session log's record types and the pure helpers an embedder resumes
//! with. Where the records are stored is the embedder's
//! ([`crate::persistence::RecordSink`]); `skutter` keeps them in JSONL files
//! (#932).

use entanglement_core::SessionId;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

mod pairing;
mod payload;
mod prefix;
pub use pairing::pair_records;
pub use payload::LogPayload;
pub use prefix::{resumable_prefix, ResumablePrefix, Truncation};

/// A single record in the session log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    /// Unix timestamp in milliseconds.
    pub ts: u64,
    /// The session this record belongs to.
    pub session: SessionId,
    /// The actual message/event payload.
    pub payload: LogPayload,
}

impl LogRecord {
    /// Creates a new log record with the current timestamp.
    pub fn new(session: SessionId, payload: LogPayload) -> Self {
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            ts,
            session,
            payload,
        }
    }
}

/// Total records the recorder dropped, if the log carries any [`LogPayload::Gap`]
/// tombstone — `None` for an intact log. Callers about to resume must refuse a
/// `Some(_)`: a gap means a contiguous run of events is missing, so replay would
/// silently reconstruct a wrong `Context` (#104).
pub fn integrity_gap(records: &[LogRecord]) -> Option<u64> {
    let mut dropped = 0u64;
    let mut any = false;
    for n in records.iter().filter_map(|r| r.payload.gap_dropped()) {
        any = true;
        dropped = dropped.saturating_add(n);
    }
    any.then_some(dropped)
}

#[cfg(test)]
mod tests_log;
