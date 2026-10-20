//! Recording one file change: store the prior bytes as a blob (unless a cap
//! says no) and append a `change` line.

use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::SessionId;

use super::manifest::{self, Change, Line, Prior, Stats, BLOBS, MANIFEST};
use super::{current_exec_scope, lock, now_ms, sha256_hex, BackupLimits, ExecScope, FileBackups};
use crate::config::undo::format_size;

/// What a change must know from *before* its write lands (ADR-0236 §2):
/// the moment, and the live tree its session is in. Taken first, a move that
/// races the write can only make the change look pre-move — at worst a
/// revert misses it, never undoes another tree's write.
#[derive(Debug, Clone)]
pub struct WriteStamp {
    session: SessionId,
    ts: u64,
    ancestry: Vec<SessionId>,
}

impl FileBackups {
    /// Stamp a write `session` is about to make. Always asks the directory,
    /// even with undo disabled: a config reload can enable it before the
    /// record, which must not then go without its tree.
    pub fn stamp(&self, session: &SessionId) -> WriteStamp {
        let ts = now_ms();
        WriteStamp {
            session: session.clone(),
            ts,
            ancestry: self.ancestry_of(session),
        }
    }

    /// [`record_stamped`](Self::record_stamped) with a stamp taken now, after
    /// the write — for callers that cannot stamp first.
    pub fn record(
        &self,
        session: &SessionId,
        request_id: &str,
        abs: &Path,
        shown: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        self.record_stamped(&self.stamp(session), request_id, abs, shown, before, after);
    }

    /// Record one file change by the stamped session's tool call
    /// `request_id`: the resolved target `abs`, the path as the call named
    /// it, the content before (`None` = no file) and after (`None` = no
    /// file). Best-effort — the edit already happened, so a backup failure is
    /// logged, never surfaced as a tool error; over-cap or failed blob writes
    /// are recorded as skipped, so a revert can say why it cannot restore
    /// them.
    pub fn record_stamped(
        &self,
        stamp: &WriteStamp,
        request_id: &str,
        abs: &Path,
        shown: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        if let Err(e) = self.try_record(stamp, request_id, abs, shown, before, after) {
            tracing::warn!("file backup of `{shown}` failed: {e:#}");
        }
    }

    fn try_record(
        &self,
        stamp: &WriteStamp,
        request_id: &str,
        abs: &Path,
        shown: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) -> Result<()> {
        let limits = self.limits();
        if !limits.enabled {
            return Ok(());
        }
        let session = &stamp.session;
        let root = self.root_of(session);
        let dir = self.inner.store.backups_dir(&root)?;
        let blobs = dir.join(BLOBS);
        let manifest_path = dir.join(MANIFEST);
        let mut manifests = lock(&self.inner.manifests);
        if !manifests.contains_key(&root) {
            std::fs::create_dir_all(&blobs)
                .with_context(|| format!("creating {}", blobs.display()))?;
            let stats = Stats::of(&manifest::read(&manifest_path)?);
            manifests.insert(root.clone(), stats);
        }
        let stats = manifests
            .get_mut(&root)
            .context("backup stats vanished under the lock")?;
        let before = match before {
            None => Prior::Absent,
            Some(bytes) => store_prior(&limits, &blobs, stats, bytes),
        };
        let scope = current_exec_scope();
        let change = Change {
            id: stats.next_id,
            ts: stamp.ts,
            session: session.clone(),
            request_id: request_id.to_string(),
            exec_seq: match scope {
                Some(ExecScope::Seq(seq)) => Some(seq),
                _ => None,
            },
            adopted: scope == Some(ExecScope::Adopted),
            path: real_path(abs),
            display: shown.to_string(),
            before,
            after: after.map(sha256_hex),
            mode: file_mode(abs),
            ancestry: stamp.ancestry.clone(),
        };
        manifest::append(&manifest_path, &Line::Change(change.clone()))?;
        stats.note(&change);
        Ok(())
    }
}

/// Store `bytes` as a blob unless a cap in `limits` says no; a failed blob
/// write degrades to `Skipped` rather than losing the manifest entry.
fn store_prior(limits: &BackupLimits, blobs: &Path, stats: &Stats, bytes: &[u8]) -> Prior {
    let size = bytes.len() as u64;
    let hash = sha256_hex(bytes);
    if size > limits.max_file_bytes {
        return Prior::Skipped {
            reason: format!(
                "larger than the {} per-file backup cap",
                format_size(limits.max_file_bytes)
            ),
        };
    }
    if stats.blobs.contains_key(&hash) {
        return Prior::Stored { hash, size };
    }
    if stats.stored_bytes().saturating_add(size) > limits.max_root_bytes {
        return Prior::Skipped {
            reason: format!(
                "the session's {} backup budget is used up",
                format_size(limits.max_root_bytes)
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

/// `abs` with its directory resolved to its real location, so a restore can
/// tell when that directory was later swapped for a symlink (an in-tree
/// symlinked directory is fine — it is recorded as where it really points).
/// The file name itself stays as given: the target may be a symlink the
/// rename replaces, never one to resolve.
fn real_path(abs: &Path) -> std::path::PathBuf {
    match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|p| p.join(name))
            .unwrap_or_else(|_| abs.to_path_buf()),
        _ => abs.to_path_buf(),
    }
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

/// The file's Unix permission bits, read after the tool's write — an
/// overwrite keeps the original mode, so this is also the mode it had.
#[cfg(unix)]
pub(super) fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|m| m.permissions().mode())
}

#[cfg(not(unix))]
pub(super) fn file_mode(_path: &Path) -> Option<u32> {
    None
}
