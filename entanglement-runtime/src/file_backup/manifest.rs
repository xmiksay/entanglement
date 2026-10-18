//! The per-root backup manifest (ADR-0212): an append-only JSONL of every
//! tracked file change plus revert markers, beside a content-addressed
//! `blobs/` directory holding each change's prior bytes.

use std::collections::{HashMap, HashSet};
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
    /// The outcome of one revert. `ids` were rolled back (or can never be:
    /// no backup) — later reverts skip them. `attempted` were selected but
    /// left alone because the file changed since; they stay revertable, but
    /// an "undo" no longer lets them pin its target turn.
    Reverted {
        ids: Vec<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attempted: Vec<u64>,
    },
}

/// One `write`/`edit`/`apply_patch` mutation of one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Change {
    pub id: u64,
    /// Unix ms when the change was recorded — the last-resort fallback for
    /// placing it in a turn.
    pub ts: u64,
    pub session: SessionId,
    /// The tool call that made it.
    pub request_id: String,
    /// The `seq` of the `ToolExec` being executed when the change was
    /// recorded — `(session, exec_seq)` names exactly one log record, which
    /// places the change in its turn. `None` off the executor's call task (a
    /// direct tool run): placed by `request_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_seq: Option<u64>,
    /// Absolute, resolved target — what a revert writes back to.
    pub path: PathBuf,
    /// The path as the tool call named it — what a report shows.
    pub display: String,
    pub before: Prior,
    /// Hex SHA-256 of what the agent left, `None` when it left no file. A
    /// revert refuses a file whose current content no longer hashes to this.
    pub after: Option<String>,
    /// Unix permission bits the file had, so a recreated file gets them back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// `session` and its ancestors in the live tree just before the write,
    /// nearest first (ADR-0236). A revert of session `T` takes the change iff
    /// `T` is in here. Empty in a manifest written before #870, with no
    /// engine to ask, or for a session the directory no longer knew (closed
    /// or hibernated, e.g. its background script writing on): then the
    /// session's latest parent edge in the logs decides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestry: Vec<SessionId>,
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

/// A manifest read back: every change in record order, the ids consumed by a
/// revert, and those a revert attempted but had to leave.
#[derive(Debug, Default)]
pub(super) struct Manifest {
    pub changes: Vec<Change>,
    pub reverted: HashSet<u64>,
    pub attempted: HashSet<u64>,
}

/// What recording needs from a root's manifest, kept in memory so a write
/// never re-reads the whole file: the next id and the stored blobs.
#[derive(Debug, Default)]
pub(super) struct Stats {
    pub next_id: u64,
    pub blobs: HashMap<String, u64>,
}

impl Stats {
    pub(super) fn of(manifest: &Manifest) -> Self {
        let mut stats = Self::default();
        for c in &manifest.changes {
            stats.note(c);
        }
        stats
    }

    pub(super) fn note(&mut self, change: &Change) {
        self.next_id = self.next_id.max(change.id + 1);
        if let Prior::Stored { hash, size } = &change.before {
            self.blobs.insert(hash.clone(), *size);
        }
    }

    /// Bytes held in `blobs/`, each distinct blob counted once.
    pub(super) fn stored_bytes(&self) -> u64 {
        self.blobs.values().sum()
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
            Ok(Line::Reverted { ids, attempted }) => {
                manifest.reverted.extend(ids);
                manifest.attempted.extend(attempted);
            }
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
