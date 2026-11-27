//! Shared atomic file writer for the managed config files (#323).
//!
//! Factored out of [`super::env_key`] so the managed env file (#304) and the
//! managed agent-models file (#323, [`super::agent_models`]) share one
//! write-then-rename primitive rather than each carrying a copy.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Write `contents` to `path` atomically: a sibling temp file (same directory, so
/// the rename stays on one filesystem), `0o600` on unix, then rename over the
/// target. A failed write cleans the temp file up rather than leaving a stray.
///
/// The temp file is `fsync`ed before the rename and the parent directory is
/// `fsync`ed after it — on Linux, a rename is only durable once the directory
/// entry pointing at the new inode is itself flushed, so skipping either sync
/// can leave `path` zero-length (or pointing at the old file) after a crash.
/// For managed files holding non-recreatable secrets (e.g. `mcp-tokens.yml`'s
/// OAuth refresh tokens, #549) that gap is worth the extra syscalls.
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
        file.sync_all()
            .with_context(|| format!("fsyncing temp file {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
        sync_dir(dir).with_context(|| format!("fsyncing directory {}", dir.display()))
    })();

    if result.is_err() {
        // Best-effort cleanup so a failed write never leaves a stray temp file.
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// Flush a directory's own metadata (the rename that just landed in it) to
/// disk. Windows has no directory handle to fsync — NTFS journals the rename
/// itself, so this is a no-op there.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_content_and_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed.yml");

        atomic_write(&path, "hello: world\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello: world\n");
        let stray_temp_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != path)
            .collect();
        assert!(
            stray_temp_files.is_empty(),
            "expected no leftover temp files, found {stray_temp_files:?}"
        );

        // A second write over the existing file still succeeds (rename over
        // an existing target, not just a fresh one).
        atomic_write(&path, "hello: again\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello: again\n");
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.yml");

        atomic_write(&path, "token: shh\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
