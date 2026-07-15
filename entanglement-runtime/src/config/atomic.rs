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
    }
    result
}
