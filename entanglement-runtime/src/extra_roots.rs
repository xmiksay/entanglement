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
//! `glob`/`grep` never trigger this approval flow themselves (#482,
//! [ADR-0132](../../../docs/adr/0132-glob-grep-escape-root-search-via-durable-grant.md)):
//! a recursive search has no single path to approve, so it never forces an
//! `Ask`. Instead it **rides** an existing `read`-tool grant — a durable
//! (`Session`/`Always`) grant on a directory (or an ancestor of one) widens
//! `list_files`'s containment check to also admit matches under that directory
//! ([`is_durably_allowed_under`][ExtraRootStore::is_durably_allowed_under]).
//! `Once` grants are deliberately excluded: a single-use token is meant to be
//! spent by the one call it was approved for, and a search can silently fan
//! out over an unbounded number of matches under the granted path — treating
//! that as "spending" a `Once` grant would let one approval cover arbitrarily
//! many reads with no further confirmation.
//!
//! # Scopes
//!
//! - **Once** — a single-use allowance bound to the specific `request_id` it was
//!   approved for (#449) and consumed by that call alone. Per-call executor
//!   tasks are detached and multiple tool tasks per session are normal, so
//!   without this binding a `Once` token approved for one call could be
//!   consumed by a *different* concurrently-running call to the same
//!   `(tool, path)` — whichever resolving call happened to reach the store
//!   first. Not persisted, not reusable.
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

/// A single-use grant: the `(tool, path)` it covers plus the `request_id` of
/// the call it was approved for (#449) — the durable scopes fall back to
/// path-only matching (`GrantKey` alone), but a `Once` token must be redeemed
/// by the exact call that earned it.
type OnceKey = (String, String, String);

#[derive(Default)]
struct Inner {
    /// Persisted across runs (`Always`).
    always: HashSet<GrantKey>,
    /// Process-lifetime (`Session`).
    session: HashSet<GrantKey>,
    /// Single-use (`Once`), removed on first consumption by its bound
    /// `request_id`.
    once: HashSet<OnceKey>,
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

    /// Whether `tool` has a **durable** grant covering `path` **or any ancestor
    /// of it** (#482) — `glob`/`grep` use this to widen a recursive search into
    /// a directory whose grant was recorded for that directory (or a parent of
    /// it), without needing a separate grant per matched file. Walks `path` and
    /// its ancestors up to the filesystem root, checking [`is_durably_allowed`]
    /// at each; `Once` grants are still excluded (inherited from
    /// `is_durably_allowed`), so a single-use approval never enables search —
    /// see the module doc.
    ///
    /// [`is_durably_allowed`]: Self::is_durably_allowed
    pub fn is_durably_allowed_under(&self, tool: &str, path: &Path) -> bool {
        let mut cur = Some(path);
        while let Some(p) = cur {
            if self.is_durably_allowed(tool, p) {
                return true;
            }
            cur = p.parent();
        }
        false
    }

    /// Whether `(tool, path)` may be accessed now by `request_id`, **consuming**
    /// a one-shot grant if that is what authorizes it. The host tools call this
    /// from the containment gate: a durable (`Session`/`Always`) grant leaves
    /// state untouched and matches on `(tool, path)` alone — the fallback for
    /// scopes that are meant to cover every later call, not just one. A `Once`
    /// grant is spent by this call **only if `request_id` matches the call it
    /// was approved for** (#449); a different concurrent call to the same
    /// `(tool, path)` cannot consume someone else's single-use token.
    pub fn take_allowance(&self, tool: &str, path: &Path, request_id: &str) -> bool {
        let k = key(tool, path);
        let mut g = self.inner.lock().expect("extra-roots mutex poisoned");
        if g.always.contains(&k) || g.session.contains(&k) {
            return true;
        }
        let once_key = (k.0, k.1, request_id.to_string());
        g.once.remove(&once_key)
    }

    /// Record an approval for `(tool, path)` at `scope`, approved by the call
    /// identified by `request_id`. `Always` also persists. `request_id` is only
    /// meaningful for `Once` (#449) — a durable scope is path-only by design, so
    /// it is ignored for `Session`/`Always`. A [`SessionDir`][ApprovalScope::SessionDir]
    /// approval (#486) has no meaning for this store's per-absolute-path key
    /// space — an escaping call's grant is out of scope for the directory
    /// widening ADR-0126 defines — so it degrades to an exact `Session` grant
    /// on this one path, same as the ordinary permission grant store does for
    /// a non-read-triad tool.
    pub fn record(&self, tool: &str, path: &Path, scope: ApprovalScope, request_id: &str) {
        let k = key(tool, path);
        let persist = {
            let mut g = self.inner.lock().expect("extra-roots mutex poisoned");
            match scope {
                ApprovalScope::Once => {
                    g.once
                        .insert((k.0.clone(), k.1.clone(), request_id.to_string()));
                    false
                }
                ApprovalScope::Session | ApprovalScope::SessionDir => {
                    g.session.insert(k.clone());
                    false
                }
                ApprovalScope::Always => {
                    g.always.insert(k.clone());
                    true
                }
            }
        };
        if persist {
            self.persist(&k);
        }
    }

    /// Merge the new `Always` grant into the managed file under an exclusive
    /// cross-process lock (#329, mirroring `grants::persist`): a concurrent
    /// skutter instance's own `Always` grant, written between this store's
    /// `load()` and now, must survive rather than being clobbered by a rewrite
    /// from stale in-memory state. Best-effort: a failure is logged, never
    /// propagated — a lost persisted grant only means the user is asked again,
    /// the safe direction.
    fn persist(&self, new_key: &GrantKey) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let result = crate::config::lock::with_locked_file(path, || {
            let mut merged: HashSet<GrantKey> = read_file(path)
                .map(|f| f.always.into_iter().collect())
                .unwrap_or_default();
            merged.insert(new_key.clone());
            let mut always: Vec<GrantKey> = merged.iter().cloned().collect();
            always.sort();
            write_file(path, &PersistedFile { always })?;
            Ok(merged)
        });
        match result {
            Ok(merged) => {
                let mut g = self.inner.lock().expect("extra-roots mutex poisoned");
                g.always.extend(merged);
            }
            Err(e) => {
                tracing::warn!("could not persist extra-roots grants to {path:?}: {e:#}");
            }
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
        s.record("read", p, ApprovalScope::Once, "req-1");
        // A different tool is not covered by a read grant.
        assert!(!s.take_allowance("write", p, "req-1"));
        // The read one-shot is spent by the first consume.
        assert!(s.take_allowance("read", p, "req-1"));
        assert!(!s.take_allowance("read", p, "req-1"));
    }

    /// #449: a `Once` grant is bound to the `request_id` it was approved for —
    /// a different concurrently-running call to the same `(tool, path)` cannot
    /// consume someone else's single-use token.
    #[test]
    fn once_is_bound_to_the_approving_request_id() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/etc/hosts");
        s.record("read", p, ApprovalScope::Once, "req-A");
        // A different, concurrently-running call cannot spend req-A's token.
        assert!(!s.take_allowance("read", p, "req-B"));
        // The token is still there — only the approved call can redeem it.
        assert!(s.take_allowance("read", p, "req-A"));
        assert!(!s.take_allowance("read", p, "req-A"), "spent once");
    }

    /// #449: two concurrent calls to the same `(tool, path)` can each be
    /// separately approved `Once` — every approval mints its own token bound to
    /// its own request id, so neither steals the other's allowance.
    #[test]
    fn concurrent_once_grants_to_the_same_path_are_independent() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/etc/hosts");
        s.record("read", p, ApprovalScope::Once, "req-A");
        s.record("read", p, ApprovalScope::Once, "req-B");
        // Redeeming req-B's token first must not also spend req-A's.
        assert!(s.take_allowance("read", p, "req-B"));
        assert!(s.take_allowance("read", p, "req-A"));
        assert!(!s.take_allowance("read", p, "req-A"));
        assert!(!s.take_allowance("read", p, "req-B"));
    }

    /// #449: the concurrency scenario from the issue, exercised with real
    /// tokio tasks racing on a shared, `Arc`-wrapped store — not just
    /// sequential calls. A `Once` grant approved for `"owner"` is hammered by
    /// many concurrent tasks impersonating *other* request ids while the real
    /// owner also races to redeem it; regardless of scheduling, only the owner
    /// ever succeeds, and it succeeds exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_calls_cannot_steal_a_once_grant_from_its_owner() {
        use std::sync::Arc;

        let s = Arc::new(ExtraRootStore::ephemeral());
        let p = std::path::PathBuf::from("/etc/hosts");
        s.record("read", &p, ApprovalScope::Once, "owner");

        let barrier = Arc::new(tokio::sync::Barrier::new(21));
        let mut tasks = Vec::new();

        // 20 impostors, all racing to steal the token with the wrong id.
        for i in 0..20 {
            let s = s.clone();
            let p = p.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                s.take_allowance("read", &p, &format!("impostor-{i}"))
            }));
        }

        // The real owner, racing alongside them.
        let owner_task = {
            let s = s.clone();
            let p = p.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                s.take_allowance("read", &p, "owner")
            })
        };

        let impostor_results: Vec<bool> = futures::future::join_all(tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let owner_result = owner_task.await.unwrap();

        assert!(
            impostor_results.iter().all(|&won| !won),
            "no impostor request id may ever consume another call's Once token"
        );
        assert!(
            owner_result,
            "the approved call must be able to redeem its own token"
        );
    }

    #[test]
    fn session_is_reusable_but_not_durable_across_take() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/var/data");
        s.record("write", p, ApprovalScope::Session, "req-1");
        assert!(s.is_durably_allowed("write", p));
        assert!(s.take_allowance("write", p, "req-1"));
        assert!(
            s.take_allowance("write", p, "some-other-request"),
            "a durable grant falls back to path-only matching, ignoring request_id"
        );
    }

    #[test]
    fn per_tool_isolation() {
        let s = ExtraRootStore::ephemeral();
        let p = Path::new("/x");
        s.record("read", p, ApprovalScope::Session, "req-1");
        assert!(s.is_durably_allowed("read", p));
        assert!(
            !s.is_durably_allowed("write", p),
            "read grant does not unlock write"
        );
    }

    /// #482: a grant on a directory widens to every path under it (and to the
    /// directory itself), but not to a sibling.
    #[test]
    fn is_durably_allowed_under_widens_to_descendants_of_a_granted_dir() {
        let s = ExtraRootStore::ephemeral();
        let dir = Path::new("/ext/lib");
        s.record("read", dir, ApprovalScope::Session, "req-1");

        assert!(s.is_durably_allowed_under("read", dir), "the dir itself");
        assert!(
            s.is_durably_allowed_under("read", &dir.join("a/b.rs")),
            "a descendant"
        );
        assert!(
            !s.is_durably_allowed_under("read", Path::new("/ext/other")),
            "a sibling must not be widened"
        );
        assert!(
            !s.is_durably_allowed_under("read", Path::new("/ext")),
            "an ancestor of the grant is not itself covered"
        );
    }

    /// #482: search-widening is per-tool, exactly like the exact-path check —
    /// a `read` grant does not widen for `write`.
    #[test]
    fn is_durably_allowed_under_is_per_tool() {
        let s = ExtraRootStore::ephemeral();
        let dir = Path::new("/ext/lib");
        s.record("read", dir, ApprovalScope::Session, "req-1");
        assert!(!s.is_durably_allowed_under("write", &dir.join("a.rs")));
    }

    /// #482: a `Once` grant must never widen a search — only `Session`/`Always`
    /// (durable) grants do, matching `is_durably_allowed`.
    #[test]
    fn is_durably_allowed_under_excludes_once_grants() {
        let s = ExtraRootStore::ephemeral();
        let dir = Path::new("/ext/lib");
        s.record("read", dir, ApprovalScope::Once, "req-1");
        assert!(!s.is_durably_allowed_under("read", &dir.join("a.rs")));
    }

    #[test]
    fn always_round_trips_through_the_file() {
        let _g = crate::config::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        std::env::set_var(EXTRA_ROOTS_FILE_ENV, &file);

        let s = ExtraRootStore::load();
        s.record(
            "read",
            Path::new("/opt/thing"),
            ApprovalScope::Always,
            "req-1",
        );

        let reloaded = ExtraRootStore::load();
        assert!(reloaded.is_durably_allowed("read", Path::new("/opt/thing")));
        assert!(!reloaded.is_durably_allowed("read", Path::new("/opt/other")));

        std::env::remove_var(EXTRA_ROOTS_FILE_ENV);
    }

    /// Two "processes" (threads, each with its own `ExtraRootStore::load()`)
    /// race to record *different* `Always` grants against the same on-disk
    /// file. Without the lock's read-current-then-merge (mirroring
    /// `grants::persist`), the second writer's rewrite of its own stale
    /// in-memory `always` set would clobber the first writer's grant — a lost
    /// update. A freshly loaded third store must see both.
    #[test]
    fn concurrent_always_grants_from_two_stores_both_survive() {
        let _g = crate::config::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra-roots.yml");
        std::env::set_var(EXTRA_ROOTS_FILE_ENV, &file);

        let a = std::thread::spawn(|| {
            let s = ExtraRootStore::load();
            s.record("read", Path::new("/ext/a"), ApprovalScope::Always, "req-a");
        });
        let b = std::thread::spawn(|| {
            let s = ExtraRootStore::load();
            s.record("write", Path::new("/ext/b"), ApprovalScope::Always, "req-b");
        });
        a.join().unwrap();
        b.join().unwrap();

        let reloaded = ExtraRootStore::load();
        assert!(
            reloaded.is_durably_allowed("read", Path::new("/ext/a")),
            "grant recorded by the first store must survive a concurrent write"
        );
        assert!(
            reloaded.is_durably_allowed("write", Path::new("/ext/b")),
            "grant recorded by the second store must survive a concurrent write"
        );

        std::env::remove_var(EXTRA_ROOTS_FILE_ENV);
    }
}
