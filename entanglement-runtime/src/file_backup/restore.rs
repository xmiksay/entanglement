//! Putting one file back (ADR-0212): conflict-checked, and atomic — the
//! restored bytes land through a synced temp file renamed over the target,
//! so a crash mid-restore never leaves a half-written file.

use std::io::Write;
use std::path::Path;

use super::manifest::{Prior, BLOBS};
use super::sha256_hex;

/// Why a file was not restored. `consumed` means it never can be (no backup):
/// the revert reports it once and marks its changes done, so it cannot pin
/// later undos. A conflict (the file changed since) stays pending.
pub(super) struct Skip {
    pub reason: String,
    pub consumed: bool,
}

impl Skip {
    fn pending(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            consumed: false,
        }
    }

    fn consumed(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            consumed: true,
        }
    }
}

/// Put `path` back to `before`, provided it still holds what the agent left
/// (`expected` hash, `None` = no file). A file already at `before` — an
/// earlier revert whose marker failed to land — counts as restored.
pub(super) fn restore(
    dir: &Path,
    path: &Path,
    before: &Prior,
    expected: Option<&str>,
    mode: Option<u32>,
) -> Result<(), Skip> {
    if let Prior::Skipped { reason } = before {
        return Err(Skip::consumed(format!("no backup ({reason})")));
    }
    let current = match std::fs::read(path) {
        Ok(bytes) => Some(sha256_hex(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(Skip::pending(format!("could not read it: {e}"))),
    };
    let already = match before {
        Prior::Absent => current.is_none(),
        Prior::Stored { hash, .. } => current.as_deref() == Some(hash.as_str()),
        Prior::Skipped { .. } => false,
    };
    if already {
        return Ok(());
    }
    if current.as_deref() != expected {
        return Err(Skip::pending("changed since the agent's edit — left as is"));
    }
    // `Skipped` returned above, so anything not `Stored` is `Absent`.
    let Prior::Stored { hash, .. } = before else {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Skip::pending(format!("could not delete it: {e}"))),
        };
    };
    let bytes = std::fs::read(dir.join(BLOBS).join(hash))
        .map_err(|e| Skip::consumed(format!("backup missing: {e}")))?;
    let mode = super::record::file_mode(path).or(mode);
    write_atomic(path, &bytes, mode).map_err(|e| Skip::pending(format!("could not write it: {e}")))
}

/// Write `bytes` to `path` via a synced sibling temp file and a rename,
/// carrying `mode` (the file's own, else the recorded one) onto it.
fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.entanglement-revert.tmp"));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        set_mode(&file, mode)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(unix)]
fn set_mode(file: &std::fs::File, mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match mode {
        Some(m) => file.set_permissions(std::fs::Permissions::from_mode(m)),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn set_mode(_file: &std::fs::File, _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}
