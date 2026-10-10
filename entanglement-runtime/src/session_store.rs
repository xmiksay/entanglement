use anyhow::{Context, Result};
use entanglement_core::SessionId;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

mod listing;
mod pairing;
mod payload;
mod prefix;
mod resume_check;
mod store;
pub use listing::{children_of, first_prompt_snippet, root_of, SessionMeta};
pub use pairing::pair_records;
pub use payload::LogPayload;
pub use prefix::{resumable_prefix, ResumablePrefix, Truncation};
pub use resume_check::{check_for_resume, resume_with_mode, Fallback, LaunchMode, ResumeCheck};
pub use store::SessionStore;

/// Sanitizes a path for safe filesystem use.
///
/// Replaces `/` and `\` with `-`, trims leading `-`.
/// Leaves all other bytes as-is (including spaces and Unicode).
///
/// # Known limitation
///
/// This is not collision-proof. Two distinct paths can map to the same folder
/// (e.g., `/a-b` and `/a/b`). A future hash-suffix disambiguator can be added
/// without breaking reads.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use entanglement_runtime::session_store::safe_cwd_name;
/// assert_eq!(safe_cwd_name(Path::new("/mnt/nvme/agent")), "mnt-nvme-agent");
/// assert_eq!(safe_cwd_name(Path::new("/a-b")), "a-b");
/// assert_eq!(safe_cwd_name(Path::new("C:\\Users\\test")), "C:-Users-test");
/// ```
pub fn safe_cwd_name(cwd: &Path) -> String {
    let path_str = cwd.to_string_lossy();
    let mut result = path_str.replace(['/', '\\'], "-");
    result = result.trim_start_matches('-').to_string();
    result
}

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

/// Reads all records from the session log at `path` — the store's own file
/// ([`SessionStore::read`]) or an exported/archived one.
///
/// Distinguishes a crash-truncated *tail* (tolerated) from *interior*
/// corruption (surfaced as an error) — see #104. A partial final line is the
/// expected result of a crash mid-`append`, so it is skipped with a warning.
/// An unparseable line with any parseable line after it is a hole (partial
/// write, bit flip) that replay would silently fold over into a wrong
/// `Context`, so it aborts the read instead. Blank lines are noise and are
/// ignored without affecting tail/interior detection.
///
/// Gap tombstones ([`LogPayload::Gap`]) parse cleanly and are returned like any
/// other record; [`integrity_gap`] inspects them so resume can refuse.
pub fn read_path(path: &Path) -> Result<Vec<LogRecord>> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;

    let reader = BufReader::new(file);
    let mut records = Vec::new();
    // An unparseable line is only tolerable as the file's tail. Hold it here
    // until we know whether another (non-blank) line follows: if one does, the
    // held line was interior corruption, not a truncated tail.
    let mut pending_corrupt: Option<String> = None;

    for line in reader.lines() {
        let line = line.with_context(|| {
            format!("Failed to read line from session file: {}", path.display())
        })?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(bad) = pending_corrupt.take() {
            return Err(anyhow::anyhow!(
                "Interior corruption in session file {}: unparseable line followed by more \
                 records — the log has a hole and cannot be safely replayed (bad line: {})",
                path.display(),
                bad
            ));
        }

        match serde_json::from_str::<LogRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) => pending_corrupt = Some(line),
        }
    }

    if let Some(bad) = pending_corrupt {
        tracing::warn!(
            "Tolerating truncated final line in {} (likely a crash mid-append): {}",
            path.display(),
            bad
        );
    }

    Ok(records)
}

/// Total records the recorder dropped, if the log carries any [`LogPayload::Gap`]
/// tombstone — `None` for an intact log. Callers about to resume must refuse a
/// `Some(_)`: a gap means a contiguous run of events is missing, so replay would
/// silently reconstruct a wrong `Context` (#104).
#[allow(dead_code)]
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
mod tests_delete_prune;
#[cfg(test)]
mod tests_log;
#[cfg(test)]
mod tests_meta;
#[cfg(test)]
mod tests_sessions;
