//! Recording one file change: store the prior bytes as a blob (unless a cap
//! says no) and append a `change` line.

use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::SessionId;

use super::manifest::{self, Change, Line, Prior, Stats, BLOBS, MANIFEST};
use super::{current_exec_seq, lock, now_ms, sha256_hex, FileBackups};

impl FileBackups {
    /// Record one file change by `session`'s tool call `request_id`: the
    /// resolved target `abs`, the path as the call named it, the content
    /// before (`None` = no file) and after (`None` = no file). Best-effort —
    /// the edit already happened, so a backup failure is logged, never
    /// surfaced as a tool error; over-cap or failed blob writes are recorded
    /// as skipped, so a revert can say why it cannot restore them.
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
        shown: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) -> Result<()> {
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
            Some(bytes) => self.store_prior(&blobs, stats, bytes),
        };
        let change = Change {
            id: stats.next_id,
            ts: now_ms(),
            session: session.clone(),
            request_id: request_id.to_string(),
            exec_seq: current_exec_seq(),
            path: abs.to_path_buf(),
            display: shown.to_string(),
            before,
            after: after.map(sha256_hex),
            mode: file_mode(abs),
        };
        manifest::append(&manifest_path, &Line::Change(change.clone()))?;
        stats.note(&change);
        Ok(())
    }

    /// Store `bytes` as a blob unless a cap says no; a failed blob write
    /// degrades to `Skipped` rather than losing the manifest entry.
    fn store_prior(&self, blobs: &Path, stats: &Stats, bytes: &[u8]) -> Prior {
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
        if stats.blobs.contains_key(&hash) {
            return Prior::Stored { hash, size };
        }
        if stats.stored_bytes() + size > self.inner.max_root_bytes {
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

fn human_bytes(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{} MiB", n / MIB)
    } else {
        format!("{n} byte")
    }
}
