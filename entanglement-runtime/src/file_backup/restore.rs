//! Putting one file back (ADR-0212): conflict-checked, and atomic — the
//! restored bytes land through a synced, exclusively created temp file
//! renamed over the target, so a crash mid-restore never leaves a
//! half-written file and a planted symlink is never written through.

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
    if directory_moved(path) {
        return Err(Skip::pending("its directory moved — left as is"));
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

/// Whether `path`'s directory no longer is where it was recorded (recorded
/// paths have a canonical directory): the nearest existing ancestor must
/// canonicalize to itself. WHY: `O_NOFOLLOW` guards only the last component —
/// a directory swapped for a symlink (`rm -r sub && ln -s /elsewhere sub`)
/// would send every read, create and rename of an unsandboxed restore out of
/// the project. Checking the nearest *existing* ancestor also keeps a later
/// `create_dir_all` from building through a symlinked prefix.
fn directory_moved(path: &Path) -> bool {
    let mut dir = path.parent();
    while let Some(d) = dir {
        match std::fs::symlink_metadata(d) {
            Ok(_) => return std::fs::canonicalize(d).ok().as_deref() != Some(d),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => dir = d.parent(),
            Err(_) => return true,
        }
    }
    false
}

/// Write `bytes` to `path` via a synced sibling temp file and a rename,
/// carrying `mode` (the file's own, else the recorded one) onto it.
fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let (mut file, tmp) = create_temp(dir)?;
    let written = (|| {
        file.write_all(bytes)?;
        set_mode(&file, mode)?;
        file.sync_all()?;
        // `rename` replaces whatever sits at `path` — a symlink included —
        // rather than writing through it.
        std::fs::rename(&tmp, path)
    })();
    match written {
        Ok(()) => {
            sync_dir(dir);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// A fresh temp file in `dir` under a short, unpredictable name. WHY not a
/// fixed name: the revert runs unsandboxed, and the model (even a sandboxed
/// `bash`) could plant a symlink at a predictable path to make the restore
/// write backup bytes outside the project. `create_new` + `O_NOFOLLOW`
/// refuses any pre-existing entry, symlink or not; a collision just retries.
/// The short name also keeps a long target filename under `NAME_MAX`.
fn create_temp(dir: &Path) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    for attempt in 0..16u32 {
        let tmp = dir.join(format!(".er-{}.tmp", temp_suffix(attempt)));
        match create_exclusive(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free temp name for the restore",
    ))
}

/// Create `path` only if nothing exists there, never following a symlink.
pub(super) fn create_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// 8 hex chars from the clock, the pid and the attempt — unpredictable
/// enough that a planted name is a miss, and `create_new` covers a hit.
fn temp_suffix(attempt: u32) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seed = format!("{nanos}:{}:{attempt}", std::process::id());
    sha256_hex(seed.as_bytes())[..8].to_string()
}

/// Make the rename itself durable — best-effort, a restore already happened.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

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
