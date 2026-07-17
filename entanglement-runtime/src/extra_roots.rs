//! Escape-root access grants (ADR-0109).
//!
//! Root containment (`resolve_under_root`, ADR-0054) is normally a hard wall: a
//! `read`/`edit`/`write` path or a `bash`/`call` `workdir` that resolves outside
//! the project root is refused outright. This store is the **approval-gated
//! exception**: when the user explicitly approves a specific out-of-root
//! `(tool, path)` — allow-once / session / always — that path is recorded here
//! and the containment gate lets *that tool* reach *that path*.
//!
//! The executor forces an `Ask` on a first out-of-root access (even if the
//! profile would `Allow`) and, on approval, records the grant here; the host
//! tools consult it when a path escapes root. Grants are **per-tool** (a `read`
//! grant does not unlock `write` on the same path) and keyed by the tool name
//! plus the normalized absolute path.
//!
//! # Scopes
//!
//! - **Once** — a single-use allowance consumed by the very next access to that
//!   `(tool, path)`. Not persisted, not reusable.
//! - **Session** — kept in memory for the life of the process. (Escape-root
//!   scope is process-wide rather than per-[`SessionId`]: this is a local,
//!   single-user tool, ADR-0047/0048, so "for this session" and "for this run"
//!   coincide in practice, and keeping the grant off the per-session dimension
//!   lets the host tools consult it without threading a session id through every
//!   `resolve_under_root` call site.)
//! - **Always** — persisted to a **managed** file
//!   (`${config_dir}/entanglement/extra-roots.yml`, override
//!   `ENTANGLEMENT_EXTRA_ROOTS_FILE`), a sibling of the grants/env files rather
//!   than a section of the hand-edited `config.yml`. Loaded at startup,
//!   re-written on each new `Always` grant. Best-effort: a write failure is
//!   logged, never fatal.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use entanglement_core::ApprovalScope;
use serde::{Deserialize, Serialize};

/// Env var overriding the managed escape-root file path (tests + non-XDG setups).
const EXTRA_ROOTS_FILE_ENV: &str = "ENTANGLEMENT_EXTRA_ROOTS_FILE";

/// A granted out-of-root access: the tool name plus the normalized absolute path
/// it may reach. Per-tool by construction — a `read` grant never satisfies a
/// `write` check.
type GrantKey = (String, String);

#[derive(Default)]
struct Inner {
    /// Persisted across runs (`Always`).
    always: HashSet<GrantKey>,
    /// Process-lifetime (`Session`).
    session: HashSet<GrantKey>,
    /// Single-use (`Once`), removed on first consumption.
    once: HashSet<GrantKey>,
}

/// Approval-gated out-of-root access grants (ADR-0109). Cheaply cloneable behind
/// an `Arc` by callers; the interior is mutex-guarded.
pub struct ExtraRootStore {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
}

fn key(tool: &str, path: &Path) -> GrantKey {
    (tool.to_string(), path.to_string_lossy().into_owned())
}

impl ExtraRootStore {
    /// Load persisted `Always` grants from the managed file (missing → empty).
    pub fn load() -> Self {
        let path = resolve_path();
        let always = path
            .as_deref()
            .and_then(read_file)
            .map(|f| f.always.into_iter().collect())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(Inner {
                always,
                ..Inner::default()
            }),
            path,
        }
    }

    /// An in-memory store with no persistence — for tests and standalone tools.
    pub fn ephemeral() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            path: None,
        }
    }

    /// Whether `(tool, path)` has a **durable** (session/always) grant. Does not
    /// consume a one-shot — the executor uses this to decide whether an
    /// out-of-root access can skip the approval prompt entirely.
    pub fn is_durably_allowed(&self, tool: &str, path: &Path) -> bool {
        let k = key(tool, path);
        let g = self.inner.lock().expect("extra-roots mutex poisoned");
        g.always.contains(&k) || g.session.contains(&k)
    }

    /// Whether `(tool, path)` may be accessed now, **consuming** a one-shot grant
    /// if that is what authorizes it. The host tools call this from the
    /// containment gate: a durable grant leaves state untouched, a `Once` grant
    /// is spent by this call.
    pub fn take_allowance(&self, tool: &str, path: &Path) -> bool {
        let k = key(tool, path);
        let mut g = self.inner.lock().expect("extra-roots mutex poisoned");
        if g.always.contains(&k) || g.session.contains(&k) {
            return true;
        }
        g.once.remove(&k)
    }

    /// Record an approval for `(tool, path)` at `scope`. `Always` also persists.
    pub fn record(&self, tool: &str, path: &Path, scope: ApprovalScope) {
        let k = key(tool, path);
        let persist = {
            let mut g = self.inner.lock().expect("extra-roots mutex poisoned");
            match scope {
                ApprovalScope::Once => {
                    g.once.insert(k);
                    false
                }
                ApprovalScope::Session => {
                    g.session.insert(k);
                    false
                }
                ApprovalScope::Always => {
                    g.always.insert(k);
                    true
                }
            }
        };
        if persist {
            self.persist();
        }
    }

    /// Best-effort atomic write of the `Always` set to the managed file.
    fn persist(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let always: Vec<GrantKey> = {
            let g = self.inner.lock().expect("extra-roots mutex poisoned");
            let mut v: Vec<GrantKey> = g.always.iter().cloned().collect();
            v.sort();
            v
        };
        let file = PersistedFile { always };
        if let Err(e) = write_file(path, &file) {
            tracing::warn!("could not persist extra-roots grants to {path:?}: {e:#}");
        }
    }
}

/// On-disk shape: only the persisted `Always` set. Session/Once are never
/// written.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedFile {
    #[serde(default)]
    always: Vec<GrantKey>,
}

fn resolve_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(EXTRA_ROOTS_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|c| c.join("entanglement").join("extra-roots.yml"))
}

fn read_file(path: &Path) -> Option<PersistedFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&text).ok()
}

fn write_file(path: &Path, file: &PersistedFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_yaml::to_string(file)?;
    crate::config::atomic::atomic_write(path, &text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_is_single_use_per_tool() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/etc/hosts");
        s.record("read", p, ApprovalScope::Once);
        // A different tool is not covered by a read grant.
        assert!(!s.take_allowance("write", p));
        // The read one-shot is spent by the first consume.
        assert!(s.take_allowance("read", p));
        assert!(!s.take_allowance("read", p));
    }

    #[test]
    fn session_is_reusable_but_not_durable_across_take() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/var/data");
        s.record("write", p, ApprovalScope::Session);
        assert!(s.is_durably_allowed("write", p));
        assert!(s.take_allowance("write", p));
        assert!(s.take_allowance("write", p), "session grant is reusable");
    }

    #[test]
    fn per_tool_isolation() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/x");
        s.record("read", p, ApprovalScope::Session);
        assert!(s.is_durably_allowed("read", p));
        assert!(
            !s.is_durably_allowed("write", p),
            "read grant does not unlock write"
        );
    }

    #[test]
    fn always_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        std::env::set_var(EXTRA_ROOTS_FILE_ENV, &file);

        let s = ExtraRootStore::load();
        s.record("read", Path::new("/opt/thing"), ApprovalScope::Always);

        let reloaded = ExtraRootStore::load();
        assert!(reloaded.is_durably_allowed("read", Path::new("/opt/thing")));
        assert!(!reloaded.is_durably_allowed("read", Path::new("/opt/other")));

        std::env::remove_var(EXTRA_ROOTS_FILE_ENV);
    }
}
