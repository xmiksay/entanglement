//! Shared atomic file writer for the managed config files (#323).
//!
//! Factored out of [`super::env_key`] so the managed env file (#304) and the
//! managed agent-models file (#323, [`super::agent_models`]) share one
//! write-then-rename primitive rather than each carrying a copy.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Millis-since-epoch of the last successful [`atomic_write`] by this process
/// (0 = never). Every managed-file writer (grants, agent-models, the env
/// file, agent tool-allowlist overrides) funnels through here, so this one
/// stamp is enough for [`recent_self_write`] to tell the definitions watcher
/// "you just saw your own write, not an external one."
static LAST_MANAGED_WRITE_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure comparison behind [`recent_self_write`], split out so the boundary
/// logic is testable without a real clock or the process-global stamp (which
/// every `atomic_write` call in the test binary shares).
fn is_recent(last_ms: u64, now_ms: u64, window: Duration) -> bool {
    if last_ms == 0 {
        return false;
    }
    now_ms.saturating_sub(last_ms) < window.as_millis() as u64
}

/// Whether a managed-file write by *this process* landed within `window` of
/// now. Used by the definitions watcher (#329) to avoid re-announcing a
/// reload it caused itself — the writer already updated its own in-memory
/// state synchronously, so a watcher-triggered reload moments later is a
/// confirmed no-op from the user's perspective.
pub fn recent_self_write(window: Duration) -> bool {
    is_recent(
        LAST_MANAGED_WRITE_MS.load(Ordering::Relaxed),
        now_ms(),
        window,
    )
}

/// Write `contents` to `path` atomically: a sibling temp file (same directory, so
/// the rename stays on one filesystem), `0o600` on unix, then rename over the
/// target. A failed write cleans the temp file up rather than leaving a stray.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| ".tmp".into());
    let mut tmp_name = file_name;
    // Process id keeps concurrent `skutter` writers from colliding on the temp
    // name; the rename below is the atomic commit either way.
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp_path = dir.join(tmp_name);

    let result = (|| {
        let mut file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("creating temp file {}", tmp_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 600 {}", tmp_path.display()))?;
        }
        file.write_all(contents.as_bytes())
            .with_context(|| format!("writing temp file {}", tmp_path.display()))?;
        file.flush()
            .with_context(|| format!("flushing temp file {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))
    })();

    if result.is_err() {
        // Best-effort cleanup so a failed write never leaves a stray temp file.
        let _ = std::fs::remove_file(&tmp_path);
    } else {
        LAST_MANAGED_WRITE_MS.store(now_ms(), Ordering::Relaxed);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_recent_true_within_window_false_outside_and_when_unset() {
        assert!(!is_recent(0, 1_000, Duration::from_millis(500)));
        assert!(is_recent(1_000, 1_100, Duration::from_millis(500)));
        assert!(!is_recent(1_000, 1_600, Duration::from_millis(500)));
        // Exactly at the boundary is "not recent" (strict less-than).
        assert!(!is_recent(1_000, 1_500, Duration::from_millis(500)));
    }

    #[test]
    fn successful_write_stamps_recent_self_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed.yml");
        atomic_write(&path, "hello").unwrap();
        // A generous window avoids flakiness from other tests in this binary
        // also calling `atomic_write` concurrently (the stamp is process-wide) —
        // this only asserts the *own* write registered, not that it's exclusive.
        assert!(recent_self_write(Duration::from_secs(3600)));
    }
}
