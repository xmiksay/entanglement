use anyhow::{Context, Result};
use entanglement_core::SessionId;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod listing;
mod pairing;
mod payload;
mod resume_check;
pub use listing::{children_of, first_prompt_snippet, list_sessions, root_of, SessionMeta};
pub use pairing::pair_records;
pub use payload::LogPayload;
pub use resume_check::{load_for_resume, retired_agent, retired_agent_note};

/// Returns the base data directory for entanglement session storage.
///
/// This is `<data_dir>/entanglement/sessions`, creating it if it doesn't exist.
///
/// # Errors
///
/// Returns an error if:
/// - The data directory cannot be determined
/// - The directory cannot be created
pub fn base_dir() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Failed to determine data directory")?;

    let base = data_dir.join("entanglement/sessions");

    if !base.exists() {
        std::fs::create_dir_all(&base)
            .with_context(|| format!("Failed to create sessions directory: {}", base.display()))?;
    }

    Ok(base)
}

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

/// Returns the session directory for a given current working directory.
pub fn session_dir(cwd: &Path) -> Result<PathBuf> {
    let base = base_dir()?;
    let safe_name = safe_cwd_name(cwd);
    let dir = base.join(&safe_name);

    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create session directory: {}", dir.display()))?;
    }

    Ok(dir)
}

/// Returns the per-project scratch directory (`<session_dir>/tmp`), creating it
/// if missing. A runtime-owned, always-writable home — outside the project repo
/// and outside every watched definition tree — for tool artifacts (e.g. the
/// `call` tool's default stdout/stderr dumps) that would otherwise pollute the
/// workdir and re-trigger the definitions watcher.
pub fn scratch_dir(cwd: &Path) -> Result<PathBuf> {
    let dir = session_dir(cwd)?.join("tmp");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create scratch directory: {}", dir.display()))?;
    }
    Ok(dir)
}

/// Returns the path to a session's JSONL file.
pub fn session_path(cwd: &Path, root_session_id: &SessionId) -> Result<PathBuf> {
    let dir = session_dir(cwd)?;
    Ok(dir.join(format!("{}.jsonl", root_session_id.0)))
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

/// Appends a log record to a session file.
///
/// Creates the file if it doesn't exist.
pub fn append(cwd: &Path, root_session_id: &SessionId, record: &LogRecord) -> Result<()> {
    let path = session_path(cwd, root_session_id)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;

    let line = serde_json::to_string(record).context("Failed to serialize log record")?;
    writeln!(file, "{}", line)
        .with_context(|| format!("Failed to write to session file: {}", path.display()))?;

    Ok(())
}

/// Reads all records from a session file.
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
#[allow(dead_code)]
pub fn read(cwd: &Path, root_session_id: &SessionId) -> Result<Vec<LogRecord>> {
    read_path(&session_path(cwd, root_session_id)?)
}

/// [`read`] for a log at an explicit `path` (an exported or archived file).
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
    for record in records {
        if let LogPayload::Gap { dropped: n } = &record.payload {
            any = true;
            dropped = dropped.saturating_add(*n);
        }
    }
    any.then_some(dropped)
}

/// Deletes a single session's `.jsonl` file (Issue 4, Phase 4.1).
///
/// Removes only the file for `root_session_id` under `cwd`'s session directory —
/// it does not walk a spawn subtree (child sessions persist inside their root's
/// file). **The caller must not pass a live session id**: this function has no
/// notion of the TUI's live set, so the modal/state that does is responsible
/// for guarding against that (the sessions modal checks `app.sessions()` and
/// refuses a live id with a status line; the resume modal only ever lists past
/// sessions, so `d` there always deletes).
///
/// # Errors
///
/// Returns an error if the file does not exist (refuses rather than silently
/// treating a missing file as success, so a stale UI row is surfaced) or if the
/// underlying `remove_file` fails (permissions, etc).
pub fn delete(cwd: &Path, root_session_id: &SessionId) -> Result<()> {
    let path = session_path(cwd, root_session_id)?;
    std::fs::remove_file(&path)
        .with_context(|| format!("Failed to delete session file: {}", path.display()))?;
    Ok(())
}

/// Prunes session files older than `retention_days` from `cwd`'s session
/// directory (Issue 4, Phase 4.2).
///
/// Scope is the **current cwd's session dir only** — matching the per-cwd
/// isolation `session_dir` already enforces. It does NOT walk sibling project
/// dirs under `<data_dir>/entanglement/sessions/`.
///
/// A file's age is its mtime (`list_sessions` derives `last_active` the same
/// way). A read-only data dir or an unreadable entry is logged and skipped, not
/// fatal — startup must not fail because pruning couldn't happen. Returns the
/// count of files actually removed (best-effort telemetry).
pub fn prune(cwd: &Path, retention_days: u64) -> Result<usize> {
    let dir = session_dir(cwd)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                "session prune: could not read {}: {} — skipping",
                dir.display(),
                e
            );
            return Ok(0);
        }
    };

    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(retention_days * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut removed = 0usize;
    for entry in entries {
        let path = match entry {
            Ok(e) => e.path(),
            Err(e) => {
                tracing::warn!("session prune: skipping unreadable entry: {}", e);
                continue;
            }
        };
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    "session prune: skipping {} (no mtime): {}",
                    path.display(),
                    e
                );
                continue;
            }
        };
        if mtime > cutoff {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!("session prune: removed stale {}", path.display());
                removed += 1;
            }
            Err(e) => {
                tracing::warn!("session prune: could not remove {}: {}", path.display(), e);
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests_delete_prune;
#[cfg(test)]
mod tests_log;
#[cfg(test)]
mod tests_meta;
#[cfg(test)]
mod tests_sessions;
