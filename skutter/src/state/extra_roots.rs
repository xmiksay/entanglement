//! Escape-root grants (ADR-0109, moved from the library by #968): the
//! out-of-root locations `skutter`'s escape-root check
//! ([`crate::host::escape::EscapeResolver`]) lets a tool reach without
//! asking.
//!
//! A tool call that reaches outside the project root asks through the
//! resolver, and its approval is an ordinary mode-scoped grant on the call's
//! `(tool, argument)` (the library's `GrantStore`). What lands here instead
//! is what no single call approves: a `/cd` into an out-of-root directory
//! (recorded for `bash`, whose default cwd it becomes), the trusted scratch
//! dir, and — read only — the `Always` grants earlier releases persisted.
//! Grants are per tool and keyed by the normalized absolute path.
//!
//! # The trusted scratch dir (#524, ADR-0142, amends ADR-0109)
//!
//! The per-project scratch dir (`SessionStore::scratch_dir`) is
//! **pre-trusted**: any path under it (or the dir itself) needs no approval,
//! for every tool, in every mode. It is a trusted *location*, not a per-tool
//! approval, and never persisted — re-derived from the cwd at every start.
//!
//! # Scopes
//!
//! - **Session** — kept in memory for the life of the process.
//! - **Always** — kept in memory and in the managed `extra-roots.yml`, a
//!   sibling of the grants file in the state dir (`ENTANGLEMENT_STATE_DIR`,
//!   ADR-0213), never a section of the hand-edited `config.yml`.
//!   Best-effort: a write failure is logged, never fatal.
//!
//! A `Once` approval cannot back a grant that every later call relies on,
//! so it records nothing here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use entanglement_core::ApprovalScope;
use serde::{Deserialize, Serialize};

use crate::config::atomic::atomic_write;
use crate::config::lock::with_locked_file;
use crate::config::state_dir::{state_file, EXTRA_ROOTS_FILE};

/// A granted out-of-root location: the tool name plus the normalized
/// absolute path it may reach.
type GrantKey = (String, String);

#[derive(Default)]
struct Inner {
    always: HashSet<GrantKey>,
    session: HashSet<GrantKey>,
}

/// On-disk shape of `extra-roots.yml`: the persisted `Always` set.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedFile {
    #[serde(default)]
    always: Vec<(String, String)>,
}

/// The escape-root grants; see the module docs.
pub struct ExtraRootStore {
    inner: Mutex<Inner>,
    /// The `extra-roots.yml` `Always` grants persist to; `None` keeps them
    /// in memory.
    file: Option<PathBuf>,
    /// The trusted per-project scratch dir (#524), canonical.
    scratch: Option<PathBuf>,
}

fn key(tool: &str, path: &Path) -> GrantKey {
    (tool.to_string(), path.to_string_lossy().into_owned())
}

impl ExtraRootStore {
    /// An in-memory store with no persistence — `Always` lasts the process.
    pub fn ephemeral() -> Self {
        Self::at(None)
    }

    /// The store over the managed `extra-roots.yml`.
    pub fn managed() -> Self {
        Self::at(state_file(EXTRA_ROOTS_FILE))
    }

    /// The store over an explicit file — the injection seam (#747): its
    /// `Always` grants load now, and each new one is merged into it.
    pub fn at(file: Option<PathBuf>) -> Self {
        let always = file
            .as_deref()
            .and_then(read_file)
            .map(|f| f.always.into_iter().collect())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(Inner {
                always,
                session: HashSet::new(),
            }),
            file,
            scratch: None,
        }
    }

    /// Trust `scratch` (and every path under it) for every tool (#524).
    /// Canonicalized so it matches the symlink-resolved targets the
    /// containment check compares; kept verbatim if it doesn't exist yet.
    pub fn with_scratch(mut self, scratch: PathBuf) -> Self {
        self.scratch = Some(scratch.canonicalize().unwrap_or(scratch));
        self
    }

    /// Whether `tool` may reach `path` with no approval: a durable grant
    /// for exactly `(tool, path)`, or `path` under the trusted scratch dir.
    pub fn is_durably_allowed(&self, tool: &str, path: &Path) -> bool {
        if self.scratch.as_deref().is_some_and(|s| path.starts_with(s)) {
            return true;
        }
        let k = key(tool, path);
        let g = self.inner.lock().expect("extra-roots mutex poisoned");
        g.always.contains(&k) || g.session.contains(&k)
    }

    /// Record a `Session` or `Always` grant for `(tool, path)`; `Always`
    /// also persists. A `Once` records nothing.
    pub fn record(&self, tool: &str, path: &Path, scope: ApprovalScope) {
        let k = key(tool, path);
        {
            let mut g = self.inner.lock().expect("extra-roots mutex poisoned");
            match scope {
                ApprovalScope::Once => return,
                ApprovalScope::Session => {
                    g.session.insert(k);
                    return;
                }
                ApprovalScope::Always => {
                    g.always.insert(k.clone());
                }
            }
        }
        self.persist(&k);
    }

    /// Merge the new `Always` grant into the file and adopt everything it
    /// holds — a concurrent `skutter`'s grant included. Best-effort: a lost
    /// persisted grant only means the user is asked again.
    fn persist(&self, new_key: &GrantKey) {
        let Some(file) = self.file.as_deref() else {
            return;
        };
        match add(file, new_key) {
            Ok(all) => self
                .inner
                .lock()
                .expect("extra-roots mutex poisoned")
                .always
                .extend(all),
            Err(e) => tracing::warn!("could not persist an extra-roots grant: {e:#}"),
        }
    }
}

/// Merge `new_key` into `file` under an exclusive cross-process lock (#329)
/// — read-current-then-merge, so a concurrent instance's grant written since
/// this one loaded survives — and return every persisted grant.
fn add(file: &Path, new_key: &GrantKey) -> Result<Vec<GrantKey>> {
    with_locked_file(file, || {
        let mut merged: HashSet<GrantKey> = read_file(file)
            .map(|f| f.always.into_iter().collect())
            .unwrap_or_default();
        merged.insert(new_key.clone());
        let mut always: Vec<GrantKey> = merged.into_iter().collect();
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

/// Missing or unreadable → `None`.
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

    fn store(file: &Path) -> ExtraRootStore {
        ExtraRootStore::at(Some(file.to_path_buf()))
    }

    #[test]
    fn always_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        store(&file).record("read", Path::new("/opt/thing"), ApprovalScope::Always);
        let reloaded = store(&file);
        assert!(reloaded.is_durably_allowed("read", Path::new("/opt/thing")));
        assert!(!reloaded.is_durably_allowed("read", Path::new("/opt/other")));
    }

    /// Two "processes" (threads, each with its own store) race to record
    /// *different* `Always` grants against the same file. Without the lock's
    /// read-current-then-merge, the second writer's rewrite of its own stale
    /// set would clobber the first writer's grant — a lost update.
    #[test]
    fn concurrent_always_grants_from_two_stores_both_survive() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        let (fa, fb) = (file.clone(), file.clone());
        let a = std::thread::spawn(move || {
            store(&fa).record("read", Path::new("/ext/a"), ApprovalScope::Always);
        });
        let b = std::thread::spawn(move || {
            store(&fb).record("write", Path::new("/ext/b"), ApprovalScope::Always);
        });
        a.join().unwrap();
        b.join().unwrap();
        let reloaded = store(&file);
        assert!(reloaded.is_durably_allowed("read", Path::new("/ext/a")));
        assert!(reloaded.is_durably_allowed("write", Path::new("/ext/b")));
    }

    /// The other process's grant arrives in the recording store too.
    #[test]
    fn a_new_grant_adopts_what_another_process_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        let mine = store(&file);
        store(&file).record("read", Path::new("/ext/b"), ApprovalScope::Always);
        mine.record("read", Path::new("/ext/a"), ApprovalScope::Always);
        assert!(mine.is_durably_allowed("read", Path::new("/ext/b")));
    }

    /// Only `Always` reaches the file; `Session` lasts the process and
    /// `Once` records nothing.
    #[test]
    fn scopes_land_where_they_last() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        let s = store(&file);
        s.record("bash", Path::new("/opt/a"), ApprovalScope::Session);
        s.record("bash", Path::new("/opt/b"), ApprovalScope::Once);
        assert!(s.is_durably_allowed("bash", Path::new("/opt/a")));
        assert!(!s.is_durably_allowed("bash", Path::new("/opt/b")));
        assert!(!store(&file).is_durably_allowed("bash", Path::new("/opt/a")));
    }

    #[test]
    fn grants_are_per_tool() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/x");
        s.record("read", p, ApprovalScope::Session);
        assert!(s.is_durably_allowed("read", p));
        assert!(!s.is_durably_allowed("write", p));
    }

    /// #524: the scratch dir and its descendants are allowed for every tool
    /// with no grant; a sibling is not.
    #[test]
    fn the_scratch_dir_is_trusted_for_every_tool() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        let sibling = dir.path().join("sibling");
        std::fs::create_dir_all(scratch.join("a")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let s = ExtraRootStore::ephemeral().with_scratch(scratch.clone());
        let scratch = scratch.canonicalize().unwrap();
        for tool in ["read", "write", "bash"] {
            assert!(s.is_durably_allowed(tool, &scratch), "{tool}");
        }
        assert!(s.is_durably_allowed("write", &scratch.join("a/out.txt")));
        assert!(!s.is_durably_allowed("write", &sibling.canonicalize().unwrap()));
    }
}
