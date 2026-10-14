//! [`SessionStore`]: where one project's session logs live (#747).

use anyhow::{Context, Result};
use entanglement_core::SessionId;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{read_path, safe_cwd_name, LogRecord};

/// One project's session-log directory: one `{root_id}.jsonl` per root
/// session, plus the runtime's `tmp/` scratch dir.
///
/// Injected rather than re-resolved from the data dir at every call (#747), so
/// a test (or an embedder keeping its logs elsewhere) pins its own directory
/// and never reads, writes or prunes the developer's real session logs. The
/// in-tree heads use [`SessionStore::for_cwd`], the
/// `<data_dir>/entanglement/sessions/<safe cwd>` layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStore {
    /// `None` when the platform has no data dir (or [`unavailable`]): every
    /// operation then fails with the same "cannot determine data directory"
    /// error it always did, at use time, rather than making construction
    /// fallible for every head.
    ///
    /// [`unavailable`]: Self::unavailable
    dir: Option<PathBuf>,
}

impl SessionStore {
    /// The production layout for working directory `cwd`:
    /// `<data_dir>/entanglement/sessions/<safe_cwd_name(cwd)>`. Resolves only
    /// the path; nothing is created until first use.
    pub fn for_cwd(cwd: &Path) -> Self {
        Self {
            dir: default_base().map(|base| base.join(safe_cwd_name(cwd))),
        }
    }

    /// A store with nowhere to keep logs: every operation fails (listing
    /// nothing, naming no scratch dir) — a head that was never given one.
    pub fn unavailable() -> Self {
        Self { dir: None }
    }

    /// A store rooted directly at `dir` (created on first use).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    /// The store's directory, creating it if missing.
    pub fn dir(&self) -> Result<PathBuf> {
        let dir = self
            .dir
            .clone()
            .context("Failed to determine data directory")?;
        if !dir.exists() {
            std::fs::create_dir_all(&dir).with_context(|| {
                format!("Failed to create session directory: {}", dir.display())
            })?;
        }
        Ok(dir)
    }

    /// The per-project scratch directory (`<dir>/tmp`), creating it if
    /// missing. A runtime-owned, always-writable home — outside the project
    /// repo and outside every watched definition tree — for tool artifacts that
    /// would otherwise pollute the workdir and re-trigger the definitions
    /// watcher.
    pub fn scratch_dir(&self) -> Result<PathBuf> {
        let dir = self.dir()?.join("tmp");
        if !dir.exists() {
            std::fs::create_dir_all(&dir).with_context(|| {
                format!("Failed to create scratch directory: {}", dir.display())
            })?;
        }
        Ok(dir)
    }

    /// Root session `root`'s file-backup directory (#731, ADR-0212):
    /// `<dir>/backups/<root_id>`, holding the prior content of every file its
    /// session tree's `write`/`edit`/`apply_patch` calls replaced. Not created
    /// here; it lives and dies with the root's log ([`delete`](Self::delete),
    /// [`prune`](Self::prune)).
    pub fn backups_dir(&self, root: &SessionId) -> Result<PathBuf> {
        Ok(self.dir()?.join(BACKUPS_DIR).join(&root.0))
    }

    /// The path of root session `id`'s JSONL file.
    pub fn session_path(&self, id: &SessionId) -> Result<PathBuf> {
        Ok(self.dir()?.join(format!("{}.jsonl", id.0)))
    }

    /// Appends `record` to root session `id`'s file, creating it if missing.
    pub fn append(&self, id: &SessionId, record: &LogRecord) -> Result<()> {
        let path = self.session_path(id)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open session file: {}", path.display()))?;
        let line = serde_json::to_string(record).context("Failed to serialize log record")?;
        writeln!(file, "{}", line)
            .with_context(|| format!("Failed to write to session file: {}", path.display()))?;
        Ok(())
    }

    /// Restart root `id`'s retention clock without writing a record: the tap
    /// calls it on the file a moved sub-agent's history lives in when it
    /// points the adopting root at it (ADR-0234). A missing file is left
    /// missing.
    pub fn touch(&self, id: &SessionId) -> Result<()> {
        let path = self.session_path(id)?;
        match OpenOptions::new().append(true).open(&path) {
            Ok(file) => file
                .set_modified(SystemTime::now())
                .with_context(|| format!("Failed to touch session file: {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                Err(e).with_context(|| format!("Failed to open session file: {}", path.display()))
            }
        }
    }

    /// Reads every record of root session `id` — see [`read_path`] for the
    /// truncated-tail vs interior-corruption contract.
    pub fn read(&self, id: &SessionId) -> Result<Vec<LogRecord>> {
        read_path(&self.session_path(id)?)
    }

    /// Deletes root session `id`'s `.jsonl` file (Issue 4, Phase 4.1).
    ///
    /// Removes only that file — it does not walk a spawn subtree (child
    /// sessions persist inside their root's file). **The caller must not pass a
    /// live session id**: the store has no notion of the TUI's live set, so the
    /// modal/state that does is responsible for guarding against that.
    ///
    /// # Errors
    ///
    /// Errors if the file does not exist (refusing rather than treating a
    /// missing file as success, so a stale UI row is surfaced) or if
    /// `remove_file` fails.
    pub fn delete(&self, id: &SessionId) -> Result<()> {
        let path = self.session_path(id)?;
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to delete session file: {}", path.display()))?;
        remove_backups(&self.dir()?, &id.0);
        Ok(())
    }

    /// Prunes session files older than `retention_days` (Issue 4, Phase 4.2).
    ///
    /// Scope is **this store's directory only** — for [`for_cwd`](Self::for_cwd)
    /// the current project's, never a sibling project's. A file's age is its
    /// mtime (`list_sessions` derives `last_active` the same way). An
    /// unreadable dir or entry is logged and skipped, not fatal — startup must
    /// not fail because pruning couldn't happen. Returns the count removed.
    ///
    /// Stat-only: no file is read. A file moved sub-agents were adopted from
    /// stays fresh on its own — the tap [`touch`](Self::touch)es it when it
    /// writes the pointer, and a live moved sub-agent keeps appending to it
    /// (ADR-0234). Once it does age out, resuming the adopter drops that
    /// sub-tree (`read_tree`).
    pub fn prune(&self, retention_days: u64) -> Result<usize> {
        let dir = self.dir()?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "session prune: could not read {}: {} — skipping",
                    dir.display(),
                    e
                );
                return Ok(0);
            }
        };

        let cutoff = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(retention_days * 86_400))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let mut removed = 0usize;
        for entry in entries {
            let path = match entry {
                Ok(e) => e.path(),
                Err(e) => {
                    tracing::warn!("session prune: skipping unreadable entry: {}", e);
                    continue;
                }
            };
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        "session prune: skipping {} (no mtime): {}",
                        path.display(),
                        e
                    );
                    continue;
                }
            };
            if mtime > cutoff {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::info!("session prune: removed stale {}", path.display());
                    removed += 1;
                }
                Err(e) => {
                    tracing::warn!("session prune: could not remove {}: {}", path.display(), e);
                }
            }
        }
        prune_orphan_backups(&dir);
        Ok(removed)
    }
}

/// Subdirectory of a store holding each root's file backups (ADR-0212).
const BACKUPS_DIR: &str = "backups";

/// Remove root `id`'s backup directory, if any — best-effort like `prune`
/// itself: a leftover directory is swept by the next prune.
fn remove_backups(store_dir: &Path, id: &str) {
    let path = store_dir.join(BACKUPS_DIR).join(id);
    match std::fs::remove_dir_all(&path) {
        Ok(()) => tracing::info!("removed file backups {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("could not remove backups {}: {}", path.display(), e),
    }
}

/// Backups follow their root's log (ADR-0212): any backup directory whose
/// `{root_id}.jsonl` is gone — pruned above, deleted, or removed by hand — is
/// removed too, so retention needs no clock of its own.
fn prune_orphan_backups(store_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(store_dir.join(BACKUPS_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !store_dir.join(format!("{name}.jsonl")).exists() {
            remove_backups(store_dir, &name);
        }
    }
}

/// `<data_dir>/entanglement/sessions`, the base every project's store lives
/// under. Guarded under `cfg(test)` (#747): a unit test must inject
/// `SessionStore::at(<tempdir>)`, never read, write and prune the developer's
/// live session logs.
fn default_base() -> Option<PathBuf> {
    crate::user_dirs::data_dir().map(|d| d.join("entanglement/sessions"))
}
