//! File-change audit log and optimistic concurrency tracking.
//!
//! The [`FileChangeLog`] tracks all file mutations (edits, creates, plugin runs)
//! within a session and maintains a rolling SHA256 baseline per file for stale-
//! file detection (optimistic concurrency). This is load-bearing for persistence
//! and replay: the log lives in the [`OutEvent::FileChange`] event stream, and
//! the baseline ensures edits are rejected if the file was modified externally
//! between read and write.
//!
//! # Invariants
//!
//! - Every edit must have a prior read establishing a baseline (edit without read
//!   fails with an actionable error message).
//! - The on-disk file's SHA256 must match the baseline at edit time (external
//!   modification detection).
//! - Records are appended in emission order; `seq` is monotonic per session.
//! - [`Vec<u8>`] for content (not `String`) keeps the log content-agnostic; the
//!   TUI renders via `String::from_utf8_lossy` + `diffy::create_patch`.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Kind of file change. `ApplyDiff` and `Plugin` are reserved for future work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChangeKind {
    Edit,
    ApplyDiff,
    Plugin(String),
    Create,
}

impl From<entanglement_core::FileChangeKind> for ChangeKind {
    fn from(kind: entanglement_core::FileChangeKind) -> Self {
        match kind {
            entanglement_core::FileChangeKind::Edit => ChangeKind::Edit,
            entanglement_core::FileChangeKind::ApplyDiff => ChangeKind::ApplyDiff,
            entanglement_core::FileChangeKind::Create => ChangeKind::Create,
        }
    }
}

/// A single file change record. Emitted via [`OutEvent::FileChange`] and stored
/// in the in-memory [`FileChangeLog`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileChangeRecord {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub kind: ChangeKind,
    pub seq: u64,
}

/// In-memory projection of the file-change audit log. Maintains:
/// - `records`: all change records in emission order.
/// - `last_known`: per-file rolling SHA256 baseline (key = absolute path).
///
/// The log is session-scoped and lives as long as the session; persistence and
/// replay will consume it from the event stream (future work).
#[derive(Debug, Default)]
pub struct FileChangeLog {
    pub records: Vec<FileChangeRecord>,
    pub last_known: HashMap<PathBuf, String>,
}

impl FileChangeLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a change record and update the rolling baseline. The baseline is
    /// set from `after_sha256` if present (post-edit state), else cleared.
    pub fn append(&mut self, record: FileChangeRecord) {
        let path = PathBuf::from(&record.path);
        if let Some(ref after_hash) = record.after_sha256 {
            self.last_known.insert(path, after_hash.clone());
        } else {
            self.last_known.remove(&path);
        }
        self.records.push(record);
    }

    /// Get the current baseline hash for `path` (if any). Returns `None` if the
    /// file was never read or was deleted.
    pub fn get_baseline(&self, path: &PathBuf) -> Option<&String> {
        self.last_known.get(path)
    }

    /// Set or update the baseline for `path` (called by `read` after a successful
    /// read of the full file).
    pub fn set_baseline(&mut self, path: PathBuf, hash: String) {
        self.last_known.insert(path, hash);
    }
}

/// Compute SHA256 of `bytes` as a hex string. Used for baseline tracking and
/// external-modification detection.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty() {
        assert_eq!(sha256_bytes(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn sha256_known() {
        assert_eq!(
            sha256_bytes(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn log_append_updates_baseline() {
        let mut log = FileChangeLog::new();
        let path = PathBuf::from("/tmp/test.txt");
        let rec = FileChangeRecord {
            path: path.to_string_lossy().into_owned(),
            before: Some(b"old".to_vec()),
            after: Some(b"new".to_vec()),
            before_sha256: Some(sha256_bytes(b"old")),
            after_sha256: Some(sha256_bytes(b"new")),
            kind: ChangeKind::Edit,
            seq: 1,
        };
        log.append(rec);
        assert_eq!(log.records.len(), 1);
        assert_eq!(log.get_baseline(&path), Some(&sha256_bytes(b"new")));
    }

    #[test]
    fn set_baseline_gets_retrieved() {
        let mut log = FileChangeLog::new();
        let path = PathBuf::from("/tmp/a.txt");
        log.set_baseline(path.clone(), "hash123".to_string());
        assert_eq!(log.get_baseline(&path), Some(&"hash123".to_string()));
    }

    #[test]
    fn baseline_cleared_on_no_after_hash() {
        let mut log = FileChangeLog::new();
        let path = PathBuf::from("/tmp/b.txt");
        log.set_baseline(path.clone(), "old".to_string());
        let rec = FileChangeRecord {
            path: path.to_string_lossy().into_owned(),
            before: Some(b"x".to_vec()),
            after: None,
            before_sha256: Some("old".to_string()),
            after_sha256: None,
            kind: ChangeKind::Edit,
            seq: 1,
        };
        log.append(rec);
        assert!(log.get_baseline(&path).is_none());
    }
}