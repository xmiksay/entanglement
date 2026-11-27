//! Advisory cross-process file lock for the managed read-modify-write cycles
//! (#329): grants.yml, the managed .env, agent-models.yml. Two skutter
//! instances doing load -> mutate -> write concurrently on the same file must
//! not silently clobber each other's update.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Sibling `<name>.lock` path for `path`, held open for the whole
/// read-modify-write critical section (never the target file itself — that one
/// gets replaced wholesale by `atomic_write`'s temp+rename, which would break a
/// lock held on its inode).
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

/// Run `f` under an exclusive advisory lock on `path`'s `.lock` sibling,
/// blocking until acquired. Creates the parent dir + lock file if missing.
/// `f` should re-read the target file's current on-disk state itself (the lock
/// only serializes callers; it does not cache anything) so it merges against
/// the latest writer, not a stale snapshot taken before the lock was acquired.
pub fn with_locked_file<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lp = lock_path(path);
    if let Some(parent) = lp.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating lock dir {}", parent.display()))?;
    }
    // The lock file's content is never read — only its inode is locked — so
    // `truncate(false)` is deliberate: nothing to overwrite, and truncating
    // would pointlessly dirty the (already-open, un-inspected) file on every
    // acquisition.
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lp)
        .with_context(|| format!("opening lock file {}", lp.display()))?;
    let mut rw_lock = fd_lock::RwLock::new(file);
    let _guard = rw_lock
        .write()
        .with_context(|| format!("acquiring lock on {}", lp.display()))?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn lock_path_is_sibling_dot_lock() {
        let p = Path::new("/tmp/entanglement/grants.yml");
        assert_eq!(lock_path(p), Path::new("/tmp/entanglement/grants.yml.lock"));
    }

    #[test]
    fn serializes_concurrent_critical_sections() {
        // Two threads racing `with_locked_file` on the same path must never
        // interleave: each increment-then-read-back pair stays atomic, so the
        // final counter reflects both increments (a broken lock would show a
        // lost update under contention often enough to flake this test).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counter.yml");
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let counter = counter.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    with_locked_file(&path, || {
                        let before = counter.load(Ordering::SeqCst);
                        std::thread::yield_now();
                        counter.store(before + 1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 8 * 25);
    }
}
