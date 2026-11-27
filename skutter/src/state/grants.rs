//! Persisted "always allow" tool grants (#174) — `skutter`'s [`GrantStore`].
//!
//! An [`InMsg::Approve`][entanglement_core::InMsg::Approve] carries an
//! [`ApprovalScope`]: [`Once`][ApprovalScope::Once] is the historical one-shot
//! approval (the next identical call asks again); [`Session`][ApprovalScope::Session]
//! and [`Always`][ApprovalScope::Always] widen it so an *identical* later call —
//! same tool **and** the argument the runtime's `grading_arg` extracts
//! (command/path, #173; root-relativized for path tools, #485, ADR-0125) —
//! skips the prompt.
//!
//! A grant only ever upgrades a resolved `Ask` to `Allow`; it never touches a
//! `Deny`, so a hard policy floor (the session's permission mode or the config
//! ceiling, #172) stands regardless of what the user once approved. Matching
//! is **exact**: a grant for `bash(git status)` re-allows only that command,
//! never `git status -s`. A `Session`/`Always` grant also carries the **mode**
//! it was earned in and matches only that mode exactly (ADR-0207 §8); a
//! pre-ADR-0207 grants file has no mode on its entries, loaded as
//! `mode: None`, which by construction never equals a live call's mode (never
//! matches, the safe direction).
//!
//! # Scopes
//!
//! - **Session** — kept in memory by the library's [`InMemoryGrantStore`].
//!   Gone when the process exits; a child session never inherits a parent's
//!   session grants.
//! - **Always** — persisted to the **managed** grants file
//!   (`<state dir>/grants.yml`, relocated by `ENTANGLEMENT_STATE_DIR`), a
//!   sibling of the provider-key env file (#220) rather than a section of the
//!   hand-edited `config.yml`. A check re-reads the file whenever its
//!   `(mtime, size)` moved, so a grant another `skutter` instance recorded —
//!   or a line the user deleted — counts at once (#329), while an unchanged
//!   file costs one `stat` and a malformed one warns once per version.
//!   Best-effort: a write failure is logged and the grant kept in memory for
//!   this process only.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use crate::config::state_dir::{state_file, GRANTS_FILE};
use async_trait::async_trait;
use entanglement_core::{ApprovalScope, SessionId};
use entanglement_runtime::policy::{GrantStore, InMemoryGrantStore};

/// A single granted tool call: the tool name, the optional argument
/// (command/path, #173) the grant was recorded against, and the permission
/// **mode** it was earned in (ADR-0207 §8) — `None` only for a legacy entry
/// loaded from a pre-ADR-0207 grants file, which never matches a live call.
/// `arg == None` grants every call to a tool that carries no permission
/// argument (e.g. `grep`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GrantKey {
    tool: String,
    arg: Option<String>,
    mode: Option<String>,
}

impl GrantKey {
    fn new(tool: &str, arg: Option<&str>, mode: &str) -> Self {
        Self {
            tool: tool.to_string(),
            arg: arg.map(str::to_string),
            mode: Some(mode.to_string()),
        }
    }

    /// The rule-key spelling used in the grants file: `tool(arg)` when scoped,
    /// bare `tool` otherwise — the same syntax the permission rules use (#173).
    fn to_rule(&self) -> String {
        match &self.arg {
            Some(a) => format!("{}({a})", self.tool),
            None => self.tool.clone(),
        }
    }

    /// Parse a grants-file rule key into a [`GrantKey`] at `mode`:
    /// `bash(git status)` ⇒ `{ bash, Some("git status"), mode }`, `grep` ⇒
    /// `{ grep, None, mode }`. A key with a `(` but no closing `)` is treated
    /// as a bare tool name (no argument).
    fn from_rule(key: &str, mode: Option<String>) -> Self {
        let (tool, arg) = match key.find('(') {
            Some(open) if key.ends_with(')') => (
                key[..open].to_string(),
                Some(key[open + 1..key.len() - 1].to_string()),
            ),
            _ => (key.to_string(), None),
        };
        Self { tool, arg, mode }
    }
}

/// `Session` grants in memory, `Always` grants in the managed file.
pub struct FileGrantStore {
    /// Session grants — and `Always` ones when there is no file, or its
    /// write failed.
    memory: InMemoryGrantStore,
    path: Option<PathBuf>,
    /// The file's grants as last parsed, keyed by the stat they were read at.
    on_disk: Mutex<Option<(FileStamp, HashSet<GrantKey>)>>,
}

/// `(mtime, size)` of the grants file; `None` when it cannot be stat'ed.
type FileStamp = Option<(Option<SystemTime>, u64)>;

fn stamp(path: &Path) -> FileStamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

impl FileGrantStore {
    /// The managed `grants.yml` in the managed-state dir
    /// (`ENTANGLEMENT_STATE_DIR`). No resolvable dir keeps every grant in
    /// memory.
    pub fn load() -> Self {
        Self::at(state_file(GRANTS_FILE))
    }

    /// A store over an explicit grants file — or, with `None`, a purely
    /// in-memory one. The injection seam (#747): a test pins a temp file
    /// rather than reading the developer's real `grants.yml`.
    pub fn at(path: Option<PathBuf>) -> Self {
        Self {
            memory: InMemoryGrantStore::new(),
            path,
            on_disk: Mutex::new(None),
        }
    }

    fn on_disk_contains(&self, path: &Path, key: &GrantKey) -> bool {
        let now = stamp(path);
        // A poisoned cache only ever holds a complete earlier parse.
        let mut cache = self.on_disk.lock().unwrap_or_else(PoisonError::into_inner);
        match cache.as_ref() {
            Some((seen, grants)) if *seen == now => grants.contains(key),
            _ => {
                let grants = read_grants(path);
                let hit = grants.contains(key);
                *cache = Some((now, grants));
                hit
            }
        }
    }
}

#[async_trait]
impl GrantStore for FileGrantStore {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>, mode: &str) -> bool {
        self.memory.is_granted(session, tool, arg, mode)
            || self
                .path
                .as_deref()
                .is_some_and(|path| self.on_disk_contains(path, &GrantKey::new(tool, arg, mode)))
    }

    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
        mode: &str,
    ) {
        if let (ApprovalScope::Always, Some(path)) = (scope, &self.path) {
            match persist(path, GrantKey::new(tool, arg, mode)) {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!("could not persist tool grants to {}: {e:#}", path.display())
                }
            }
        }
        self.memory.record(session, tool, arg, scope, mode).await;
    }

    fn forget_session(&self, session: &SessionId) {
        self.memory.forget_session(session);
    }
}

/// Add `key` to the managed file, merged against whatever is on disk under an
/// exclusive lock (#329) — a concurrent instance's own `Always` grant must
/// survive rather than being clobbered.
fn persist(path: &Path, key: GrantKey) -> anyhow::Result<()> {
    crate::config::lock::with_locked_file(path, || {
        let mut merged = read_grants(path);
        if merged.insert(key) {
            write_grants(path, &merged)?;
        }
        Ok(())
    })
}

// The managed grants file: on-disk shape, read/write.
mod file;
use file::{read_grants, write_grants};

#[cfg(test)]
mod tests;
