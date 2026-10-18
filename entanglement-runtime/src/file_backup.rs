//! Per-write file backups and the undo/rewind they enable (#731, ADR-0212).
//!
//! Before `write`/`edit`/`apply_patch` (and the `rhai` `write`/`edit`
//! bindings, which run those same tools) replace a file, the tool hands its
//! prior bytes to [`FileBackups::record`], which keeps them under the root
//! session's [`SessionStore::backups_dir`]: a content-addressed `blobs/`
//! directory plus an append-only `manifest.jsonl` naming each change's
//! session, call, absolute path and before/after state. No git involved, and
//! nothing outside the session store is written.
//!
//! [`FileBackups::revert`] — reached through core's `file_reverter` seam on an
//! `InMsg::RevertFiles` — rolls back every change a session (and its spawned
//! sub-tree) made after a given user turn: per file, it puts back the content
//! from before the *first* such change, but only if the file still holds what
//! the *last* one left. A file the user (or a `bash` command) changed since is
//! never clobbered; it is reported as skipped. Changes made through
//! `bash`/`call`/`rhai` exec are not tracked at all.
//!
//! Backups live and die with the root's session log: [`SessionStore::delete`]
//! and [`SessionStore::prune`] remove them alongside it.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use entanglement_core::{FileReverter, RevertSkip, SessionId};
use sha2::{Digest, Sha256};

use crate::session_store::SessionStore;

mod lineage;
mod load;
mod manifest;
mod record;
mod restore;
mod revert;
mod tracker;

pub use record::WriteStamp;
pub use revert::TurnEdits;
pub use tracker::{detached_work_probe, holly_ancestry, AncestryProbe, BusyProbe};

/// Default per-file cap: a prior version larger than this is not backed up
/// (its change is recorded as skipped, and a revert reports it).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Default per-root budget for stored prior versions; past it, further
/// changes are recorded as skipped.
pub const DEFAULT_MAX_ROOT_BYTES: u64 = 256 * 1024 * 1024;

/// What a [`FileBackups`] records: whether it records at all, and its size
/// caps in bytes. `skutter` reads it from `config.yml`'s `undo:` block (#787)
/// and re-applies it on a config reload ([`FileBackups::set_limits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupLimits {
    /// `false` records nothing new; backups already on disk stay restorable.
    pub enabled: bool,
    /// A prior version larger than this is recorded as skipped.
    #[serde(deserialize_with = "crate::config::undo::de_size")]
    pub max_file_bytes: u64,
    /// Per-root budget for stored prior versions; past it, further changes
    /// are recorded as skipped.
    #[serde(deserialize_with = "crate::config::undo::de_size")]
    pub max_root_bytes: u64,
}

impl Default for BackupLimits {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_root_bytes: DEFAULT_MAX_ROOT_BYTES,
        }
    }
}

tokio::task_local! {
    /// The `seq` of the `ToolExec` whose call task is running — set by the
    /// executor around each dispatched call ([`with_exec_seq`]), read when a
    /// file tool records a change so the change names its exact log record.
    static EXEC_SEQ: u64;
}

/// Run `fut` (one dispatched tool call) with its `ToolExec` seq in scope, so
/// any change it records is placed in the right turn even when request ids
/// repeat across turns (Gemini's `name#ordinal`).
pub fn with_exec_seq<F: Future>(seq: u64, fut: F) -> impl Future<Output = F::Output> {
    EXEC_SEQ.scope(seq, fut)
}

fn current_exec_seq() -> Option<u64> {
    EXEC_SEQ.try_with(|s| *s).ok()
}

/// Handle to one project's file backups. Cheap to clone; every clone shares
/// the session lineage/busy state and the per-root manifest cache.
#[derive(Clone)]
pub struct FileBackups {
    inner: Arc<Inner>,
}

struct Inner {
    store: SessionStore,
    /// session → root, folded from `SessionStarted` (see
    /// [`FileBackups::track_roots`]), so a child's backups land under its root.
    roots: Mutex<HashMap<SessionId, SessionId>>,
    /// Sessions whose last `Status` was not at rest — a revert refuses while
    /// any session of the target's sub-tree is in here.
    busy: Mutex<HashSet<SessionId>>,
    /// Extra "still working" check for detached work no `Status` reflects
    /// (background jobs, detached `rhai` scripts).
    probe: OnceLock<BusyProbe>,
    /// The live tree a change is recorded in (ADR-0236).
    ancestry: OnceLock<AncestryProbe>,
    /// Per-root manifest stats, and the lock that serializes every manifest
    /// read-modify-append: a parallel tool batch records from several tasks.
    manifests: Mutex<HashMap<SessionId, manifest::Stats>>,
    /// Read at every record, so a config reload applies to the next write.
    limits: Mutex<BackupLimits>,
}

impl FileBackups {
    /// Backups kept under `store`, with the default size caps.
    pub fn new(store: SessionStore) -> Self {
        Self::with_backup_limits(store, BackupLimits::default())
    }

    /// Backups kept under `store` with explicit caps (bytes).
    pub fn with_limits(store: SessionStore, max_file_bytes: u64, max_root_bytes: u64) -> Self {
        Self::with_backup_limits(
            store,
            BackupLimits {
                enabled: true,
                max_file_bytes,
                max_root_bytes,
            },
        )
    }

    /// Backups kept under `store`, recording per `limits`.
    pub fn with_backup_limits(store: SessionStore, limits: BackupLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                roots: Mutex::new(HashMap::new()),
                busy: Mutex::new(HashSet::new()),
                probe: OnceLock::new(),
                ancestry: OnceLock::new(),
                manifests: Mutex::new(HashMap::new()),
                limits: Mutex::new(limits),
            }),
        }
    }

    /// The limits the next recorded change is held to.
    pub fn limits(&self) -> BackupLimits {
        *lock(&self.inner.limits)
    }

    /// Replace the limits for every clone of this handle. Changes already
    /// recorded keep their outcome: a skip stays a skip, and a stored backup
    /// stays restorable even once recording is disabled.
    pub fn set_limits(&self, limits: BackupLimits) {
        *lock(&self.inner.limits) = limits;
    }

    /// The [`FileReverter`] seam core calls for `InMsg::RevertFiles`; runs
    /// [`revert`](Self::revert) off the async runtime.
    pub fn reverter(&self) -> FileReverter {
        let this = self.clone();
        Arc::new(move |session, to_turn| {
            let this = this.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || this.revert(&session, to_turn))
                    .await
                    .map_err(|e| format!("revert task failed: {e}"))?
                    .map_err(|e| format!("{e:#}"))
            })
        })
    }
}

/// The one-line, human-readable outcome of a revert, shared by the text and
/// TUI heads. Always says what is *not* covered, so nobody mistakes a revert
/// for a full working-tree rollback.
pub fn summary(to_turn: u32, restored: &[String], skipped: &[RevertSkip]) -> String {
    if restored.is_empty() && skipped.is_empty() {
        return format!("no tracked file edits to revert after turn {to_turn}");
    }
    let mut out = format!("reverted file edits after turn {to_turn}");
    if !restored.is_empty() {
        out.push_str(&format!(" — restored {}", restored.join(", ")));
    }
    for skip in skipped {
        out.push_str(&format!("; can't restore {}: {}", skip.path, skip.reason));
    }
    out.push_str(" (bash/call/rhai exec changes are not tracked)");
    out
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// A poisoned lock only means another backup panicked mid-update; the data
/// it guards is still usable, and a backup must never panic a tool call.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_adopted;
#[cfg(test)]
mod tests_host;
#[cfg(test)]
mod tests_limits;
#[cfg(test)]
mod tests_lineage;
#[cfg(test)]
mod tests_recorded;
#[cfg(test)]
mod tests_safety;
