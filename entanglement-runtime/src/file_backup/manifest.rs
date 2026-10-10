//! The per-root backup manifest (ADR-0212): an append-only JSONL of every
//! tracked file change plus revert markers, beside a content-addressed
//! `blobs/` directory holding each change's prior bytes.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use entanglement_core::SessionId;
use serde::{Deserialize, Serialize};

pub(super) const MANIFEST: &str = "manifest.jsonl";
pub(super) const BLOBS: &str = "blobs";

/// One manifest line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Line {
    Change(Change),
    /// These changes were rolled back — a later revert skips them, so a
    /// second `/undo` moves on to the turn before instead of repeating.
    Reverted {
        ids: Vec<u64>,
    },
}

/// One `write`/`edit`/`apply_patch` mutation of one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Change {
    pub id: u64,
    /// Unix ms when the change was recorded — the fallback for placing it in
    /// a turn when its call is missing from the session log.
    pub ts: u64,
    pub session: SessionId,
    /// The tool call that made it; its `ToolExec`'s log position places the
    /// change in a turn.
    pub request_id: String,
    /// Absolute, resolved target — what a revert writes back to.
    pub path: PathBuf,
    /// The path as the tool call named it — what a report shows.
    pub display: String,
    pub before: Prior,
    /// Hex SHA-256 of what the agent left, `None` when it left no file. A
    /// revert refuses a file whose current content no longer hashes to this.
    pub after: Option<String>,
}

/// The file's state before the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum Prior {
    /// No file existed — reverting deletes it.
    Absent,
    /// Its bytes live at `blobs/<hash>`.
    Stored { hash: String, size: u64 },
    /// Not backed up (over a size cap, or the backup write failed) — the
    /// change is recorded so a revert can report it instead of silently
    /// skipping it.
    Skipped { reason: String },
}

/// A manifest read back: every change in record order, and the ids already
/// reverted.
#[derive(Debug, Default)]
pub(super) struct Manifest {
    pub changes: Vec<Change>,
    pub reverted: HashSet<u64>,
}

impl Manifest {
    pub(super) fn next_id(&self) -> u64 {
        self.changes.last().map_or(0, |c| c.id + 1)
    }

    /// Bytes held in `blobs/`, each distinct blob counted once.
    pub(super) fn stored_bytes(&self) -> u64 {
        let mut seen = HashSet::new();
        self.changes
            .iter()
            .filter_map(|c| match &c.before {
                Prior::Stored { hash, size } if seen.insert(hash.as_str()) => Some(*size),
                _ => None,
            })
            .sum()
    }

    pub(super) fn has_blob(&self, hash: &str) -> bool {
        self.changes
            .iter()
            .any(|c| matches!(&c.before, Prior::Stored { hash: h, .. } if h == hash))
    }
}

/// Read `path`; a missing manifest is an empty one. An unparseable line (a
/// crash mid-append) is skipped with a warning rather than failing the whole
/// revert — at worst that one change is not restorable.
pub(super) fn read(path: &Path) -> Result<Manifest> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Manifest::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading backup manifest {}", path.display()))
        }
    };
    let mut manifest = Manifest::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Line>(line) {
            Ok(Line::Change(c)) => manifest.changes.push(c),
            Ok(Line::Reverted { ids }) => manifest.reverted.extend(ids),
            Err(e) => tracing::warn!("skipping unreadable backup manifest line: {e}"),
        }
    }
    Ok(manifest)
}

pub(super) fn append(path: &Path, line: &Line) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening backup manifest {}", path.display()))?;
    let text = serde_json::to_string(line).context("serializing backup manifest line")?;
    writeln!(file, "{text}").with_context(|| format!("appending to {}", path.display()))
}
