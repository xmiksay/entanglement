//! `extra-roots.yml` (ADR-0109): the escape-root `Always` grants behind the
//! library's [`AlwaysRoots`] seam — a managed sibling of the grants file in
//! the state dir (`ENTANGLEMENT_STATE_DIR`, ADR-0213), never a section of
//! the hand-edited `config.yml`. Only `Always` grants are written;
//! `Session`/`Once` stay in the library's memory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use entanglement_runtime::extra_roots::AlwaysRoots;
use serde::{Deserialize, Serialize};

use crate::config::atomic::atomic_write;
use crate::config::lock::with_locked_file;
use crate::config::state_dir::{state_file, EXTRA_ROOTS_FILE};

/// On-disk shape: the persisted `Always` set.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedFile {
    #[serde(default)]
    always: Vec<(String, String)>,
}

/// The managed `extra-roots.yml`; `None` inside persists nothing.
pub struct FileExtraRoots(Option<PathBuf>);

impl FileExtraRoots {
    /// The file in the managed-state dir.
    pub fn managed() -> Self {
        Self(state_file(EXTRA_ROOTS_FILE))
    }

    /// An explicit file — the injection seam (#747).
    pub fn at(path: Option<PathBuf>) -> Self {
        Self(path)
    }
}

impl AlwaysRoots for FileExtraRoots {
    /// Missing or unreadable → empty.
    fn load(&self) -> Vec<(String, String)> {
        self.0
            .as_deref()
            .and_then(read_file)
            .map(|f| f.always)
            .unwrap_or_default()
    }

    /// Merge the new grant into the file under an exclusive cross-process
    /// lock (#329) — read-current-then-merge: a concurrent skutter instance's
    /// own `Always` grant, written since this one loaded, must survive rather
    /// than being clobbered by a rewrite from stale in-memory state.
    fn add(&self, tool: &str, path: &str) -> Result<Vec<(String, String)>> {
        let Some(file) = self.0.as_deref() else {
            return Ok(Vec::new());
        };
        with_locked_file(file, || {
            let mut merged: HashSet<(String, String)> = read_file(file)
                .map(|f| f.always.into_iter().collect())
                .unwrap_or_default();
            merged.insert((tool.to_string(), path.to_string()));
            let mut always: Vec<(String, String)> = merged.into_iter().collect();
            always.sort();
            write_file(
                file,
                &PersistedFile {
                    always: always.clone(),
                },
            )?;
            Ok(always)
        })
    }
}

fn read_file(path: &Path) -> Option<PersistedFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&text).ok()
}

fn write_file(path: &Path, file: &PersistedFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(path, &serde_yaml::to_string(file)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use entanglement_core::ApprovalScope;
    use entanglement_runtime::extra_roots::ExtraRootStore;

    fn store(file: &Path) -> ExtraRootStore {
        ExtraRootStore::ephemeral()
            .with_always_roots(Arc::new(FileExtraRoots::at(Some(file.to_path_buf()))))
    }

    #[test]
    fn always_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");

        store(&file).record(
            "read",
            Path::new("/opt/thing"),
            ApprovalScope::Always,
            "req-1",
        );

        let reloaded = store(&file);
        assert!(reloaded.is_durably_allowed("read", Path::new("/opt/thing")));
        assert!(!reloaded.is_durably_allowed("read", Path::new("/opt/other")));
    }

    /// Two "processes" (threads, each with its own store) race to record
    /// *different* `Always` grants against the same file. Without the lock's
    /// read-current-then-merge, the second writer's rewrite of its own stale
    /// set would clobber the first writer's grant — a lost update. A freshly
    /// loaded third store must see both.
    #[test]
    fn concurrent_always_grants_from_two_stores_both_survive() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");

        let (fa, fb) = (file.clone(), file.clone());
        let a = std::thread::spawn(move || {
            store(&fa).record("read", Path::new("/ext/a"), ApprovalScope::Always, "req-a");
        });
        let b = std::thread::spawn(move || {
            store(&fb).record("write", Path::new("/ext/b"), ApprovalScope::Always, "req-b");
        });
        a.join().unwrap();
        b.join().unwrap();

        let reloaded = store(&file);
        assert!(
            reloaded.is_durably_allowed("read", Path::new("/ext/a")),
            "grant recorded by the first store must survive a concurrent write"
        );
        assert!(
            reloaded.is_durably_allowed("write", Path::new("/ext/b")),
            "grant recorded by the second store must survive a concurrent write"
        );
    }

    /// The other process's grant arrives in the recording store too.
    #[test]
    fn a_new_grant_adopts_what_another_process_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        let mine = store(&file);
        store(&file).record("read", Path::new("/ext/b"), ApprovalScope::Always, "r");
        mine.record("read", Path::new("/ext/a"), ApprovalScope::Always, "r");
        assert!(mine.is_durably_allowed("read", Path::new("/ext/b")));
    }
}
