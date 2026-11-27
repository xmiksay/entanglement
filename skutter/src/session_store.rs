//! The on-disk session store (#932): one JSONL file per root session under
//! the project's data dir, its listing, resume checks and retention. The
//! record types and the pure resume helpers stay in the library
//! (`entanglement_runtime::session_store`); callers import them from there.

use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use entanglement_runtime::session_store::*;

mod compacted;
mod listing;
mod resume_check;
mod revive;
mod store;
mod tree;
pub use listing::{children_of, first_prompt_snippet, resumable_roots, root_of, SessionMeta};
pub use resume_check::{check_for_resume, resume_with_mode, Fallback, LaunchMode, ResumeCheck};
pub use revive::revived_with;
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
/// use skutter::session_store::safe_cwd_name;
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

#[cfg(test)]
mod tests_delete_prune;
#[cfg(test)]
mod tests_log;
#[cfg(test)]
mod tests_meta;
#[cfg(test)]
mod tests_sessions;
