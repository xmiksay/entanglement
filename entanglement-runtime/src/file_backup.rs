//! Per-write file backups and the undo/rewind they enable (#731, ADR-0212).
//!
//! Before `write`/`edit`/`apply_patch` (and the `rhai` `write`/`edit`
//! bindings, which run those same tools) replace a file, the tool hands its
//! prior bytes to [`FileBackups::record`], which keeps them under the root
//! session's [`SessionStore::backups_dir`]: a content-addressed `blobs/`
//! directory plus an append-only `manifest.jsonl` naming each change's
//! session, time, absolute path and before/after state. No git involved, and
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

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use entanglement_core::{FileReverter, Holly, OutEvent, RevertSkip, SessionId};
use sha2::{Digest, Sha256};

use crate::session_store::SessionStore;

mod manifest;
mod revert;

use manifest::{Change, Line, Prior, BLOBS, MANIFEST};
pub use revert::TurnEdits;

/// Default per-file cap: a prior version larger than this is not backed up
/// (its change is recorded as skipped, and a revert reports it).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Default per-root budget for stored prior versions; past it, further
/// changes are recorded as skipped.
pub const DEFAULT_MAX_ROOT_BYTES: u64 = 256 * 1024 * 1024;

/// Handle to one project's file backups. Cheap to clone; every clone shares
/// the session→root map and the write lock.
#[derive(Clone)]
pub struct FileBackups {
    inner: Arc<Inner>,
}

struct Inner {
    store: SessionStore,
    /// session → root, folded from `SessionStarted` (see [`FileBackups::track_roots`]),
    /// so a spawned child's backups land under its root's directory.
    roots: Mutex<HashMap<SessionId, SessionId>>,
    /// Serializes manifest read-modify-append: a parallel tool batch writes
    /// from several tasks at once.
    lock: Mutex<()>,
    max_file_bytes: u64,
    max_root_bytes: u64,
}

impl FileBackups {
    /// Backups kept under `store`, with the default size caps.
    pub fn new(store: SessionStore) -> Self {
        Self::with_limits(store, DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_ROOT_BYTES)
    }

    /// Backups kept under `store` with explicit caps (bytes).
    pub fn with_limits(store: SessionStore, max_file_bytes: u64, max_root_bytes: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                roots: Mutex::new(HashMap::new()),
                lock: Mutex::new(()),
                max_file_bytes,
                max_root_bytes,
            }),
        }
    }

    /// Fold `SessionStarted` lineage from `holly` into the session→root map
    /// (the same rule the persistence tap routes records by), so backups and
    /// reverts for a spawned child resolve to its root's directory and log.
    pub fn track_roots(&self, holly: &Holly) -> tokio::task::JoinHandle<()> {
        let mut events = holly.subscribe();
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(OutEvent::SessionStarted {
                        session,
                        parent,
                        root,
                        ..
                    }) => {
                        let root_id = match (root, parent) {
                            (false, Some(p)) => this.root_of(&p),
                            _ => session.clone(),
                        };
                        this.note_root(session, root_id);
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// Record that `session` belongs to root `root`.
    pub fn note_root(&self, session: SessionId, root: SessionId) {
        lock(&self.inner.roots).insert(session, root);
    }

    fn root_of(&self, session: &SessionId) -> SessionId {
        lock(&self.inner.roots)
            .get(session)
            .cloned()
            .unwrap_or_else(|| session.clone())
    }

    /// Record one file change by `session`'s tool call `request_id`: the
    /// resolved target `abs`, the path as the call named it, the content
    /// before (`None` = no file) and after (`None` = no file). Best-effort —
    /// the edit already happened, so a backup failure is logged, never
    /// surfaced as a tool error; the change is then recorded as skipped when
    /// possible, so a revert can say why it cannot restore it.
    pub fn record(
        &self,
        session: &SessionId,
        request_id: &str,
        abs: &Path,
        shown: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        if let Err(e) = self.try_record(session, request_id, abs, shown, before, after) {
            tracing::warn!("file backup of `{shown}` failed: {e:#}");
        }
    }

    fn try_record(
        &self,
        session: &SessionId,
        request_id: &str,
        abs: &Path,
        display: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) -> Result<()> {
        let _guard = lock(&self.inner.lock);
        let dir = self.inner.store.backups_dir(&self.root_of(session))?;
        let blobs = dir.join(BLOBS);
        std::fs::create_dir_all(&blobs).with_context(|| format!("creating {}", blobs.display()))?;
        let manifest_path = dir.join(MANIFEST);
        let manifest = manifest::read(&manifest_path)?;
        let before = match before {
            None => Prior::Absent,
            Some(bytes) => self.store_prior(&blobs, &manifest, bytes),
        };
        let change = Change {
            id: manifest.next_id(),
            ts: now_ms(),
            session: session.clone(),
            request_id: request_id.to_string(),
            path: abs.to_path_buf(),
            display: display.to_string(),
            before,
            after: after.map(sha256_hex),
        };
        manifest::append(&manifest_path, &Line::Change(change))
    }

    /// Store `bytes` as a blob unless a cap says no; a failed blob write
    /// degrades to `Skipped` rather than losing the manifest entry.
    fn store_prior(&self, blobs: &Path, manifest: &manifest::Manifest, bytes: &[u8]) -> Prior {
        let size = bytes.len() as u64;
        let hash = sha256_hex(bytes);
        if size > self.inner.max_file_bytes {
            return Prior::Skipped {
                reason: format!(
                    "larger than the {} per-file backup cap",
                    human_bytes(self.inner.max_file_bytes)
                ),
            };
        }
        if manifest.has_blob(&hash) {
            return Prior::Stored { hash, size };
        }
        if manifest.stored_bytes() + size > self.inner.max_root_bytes {
            return Prior::Skipped {
                reason: format!(
                    "the session's {} backup budget is used up",
                    human_bytes(self.inner.max_root_bytes)
                ),
            };
        }
        match write_blob(blobs, &hash, bytes) {
            Ok(()) => Prior::Stored { hash, size },
            Err(e) => Prior::Skipped {
                reason: format!("backup write failed: {e:#}"),
            },
        }
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

fn write_blob(blobs: &Path, hash: &str, bytes: &[u8]) -> Result<()> {
    let target = blobs.join(hash);
    if target.exists() {
        return Ok(());
    }
    // Write-then-rename: a crash mid-write must never leave a truncated blob
    // under a hash that claims to be the whole file.
    let tmp = blobs.join(format!("{hash}.tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &target).with_context(|| format!("renaming {}", tmp.display()))
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

fn human_bytes(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{} MiB", n / MIB)
    } else {
        format!("{n} byte")
    }
}

/// A poisoned lock only means another backup panicked mid-update; the data
/// it guards (a map, a unit) is still usable, and a backup must never panic
/// a tool call.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
