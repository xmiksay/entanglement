//! Escape-root access grants (ADR-0109).
//!
//! Root containment (`resolve_under_root`, ADR-0054) is normally a hard wall: a
//! `read`/`edit`/`write` path or a `bash` `workdir` that resolves outside
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
//! [ADR-0132](../../../docs/adr/archive/0132-glob-grep-escape-root-search-via-durable-grant.md)):
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
//! # The trusted scratch dir (#524, ADR-0142, amends ADR-0109)
//!
//! The runtime-owned per-project scratch dir (`SessionStore::scratch_dir`)
//! is **pre-trusted**: any path
//! under it (or the dir itself) needs no approval and no prior grant, for
//! every tool, in every profile. [`with_scratch`][ExtraRootStore::with_scratch]
//! records it once at startup; [`is_durably_allowed`]/[`take_allowance`] check
//! it *before* the ordinary per-`(tool, path)` grant lookup, so it composes for
//! free with the search-widening above (an ancestor walk that reaches the
//! scratch dir short-circuits true) with no separate wiring. Unlike an
//! ordinary escape-root grant this is **not** per-tool — the scratch dir is a
//! trusted *location*, not a one-off approval for one tool — and it is never
//! persisted: it is re-derived from the cwd at every startup, exactly like
//! `SessionStore::scratch_dir` itself.
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
//! - **Always** — kept in memory and, when the embedder plugs one in
//!   ([`with_always_roots`](ExtraRootStore::with_always_roots)), handed to
//!   an [`AlwaysRoots`] that outlives the process (`skutter`: its managed
//!   `extra-roots.yml`). Loaded at construction, recorded on each new
//!   `Always` grant. Best-effort: a write failure is logged, never fatal.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use entanglement_core::ApprovalScope;

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

/// Where `Always` grants outlive the process — the embedder's storage
/// (`skutter`: the managed `extra-roots.yml`). Grants are `(tool, path)`
/// pairs, the path normalized and absolute.
pub trait AlwaysRoots: Send + Sync {
    /// Every persisted grant.
    fn load(&self) -> Vec<(String, String)>;
    /// Persist the grant `(tool, path)` and return every persisted grant —
    /// including any another process recorded meanwhile, so it is not lost.
    fn add(&self, tool: &str, path: &str) -> anyhow::Result<Vec<(String, String)>>;
}

/// Approval-gated out-of-root access grants (ADR-0109). Cheaply cloneable behind
/// an `Arc` by callers; the interior is mutex-guarded.
pub struct ExtraRootStore {
    inner: Mutex<Inner>,
    persisted: Option<Arc<dyn AlwaysRoots>>,
    /// The trusted per-project scratch dir (#524, ADR-0142), if wired.
    /// Canonicalized at construction so it compares equal to the
    /// symlink-resolved targets `resolve_and_contained` produces. `None`
    /// (every pre-#524 constructor) disables the carve-out entirely.
    scratch: Option<PathBuf>,
}

fn key(tool: &str, path: &Path) -> GrantKey {
    (tool.to_string(), path.to_string_lossy().into_owned())
}

impl ExtraRootStore {
    /// An in-memory store with no persistence — `Always` lasts the process.
    pub fn ephemeral() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            persisted: None,
            scratch: None,
        }
    }

    /// Load `Always` grants from `persisted` now and record each new one
    /// there.
    pub fn with_always_roots(mut self, persisted: Arc<dyn AlwaysRoots>) -> Self {
        self.inner
            .get_mut()
            .expect("extra-roots mutex poisoned")
            .always
            .extend(persisted.load());
        self.persisted = Some(persisted);
        self
    }

    /// Trust `scratch` (and every path under it) for every tool, with no
    /// approval and no per-`(tool, path)` grant (#524, ADR-0142) — the
    /// runtime-owned per-project scratch dir. Canonicalized here so it matches
    /// the symlink-resolved form `resolve_and_contained` compares against;
    /// falls back to the given path verbatim if it doesn't yet exist.
    pub fn with_scratch(mut self, scratch: PathBuf) -> Self {
        self.scratch = Some(scratch.canonicalize().unwrap_or(scratch));
        self
    }

    /// Whether `path` is the trusted scratch dir or a descendant of it (#524).
    /// Tool-independent by design — the scratch dir is a trusted *location*,
    /// not a per-tool approval.
    fn is_trusted_scratch(&self, path: &Path) -> bool {
        self.scratch
            .as_deref()
            .is_some_and(|s| path == s || path.starts_with(s))
    }

    /// Whether `(tool, path)` has a **durable** (session/always) grant, *or*
    /// `path` falls under the trusted scratch dir (#524). Does not consume a
    /// one-shot — the executor uses this to decide whether an out-of-root
    /// access can skip the approval prompt entirely.
    pub fn is_durably_allowed(&self, tool: &str, path: &Path) -> bool {
        if self.is_trusted_scratch(path) {
            return true;
        }
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
        if self.is_trusted_scratch(path) {
            return true;
        }
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
    /// it is ignored for `Session`/`Always`.
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
                ApprovalScope::Session => {
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

    /// Hand the new `Always` grant to the embedder's [`AlwaysRoots`] and
    /// adopt everything it holds. Best-effort: a failure is logged, never
    /// propagated — a lost persisted grant only means the user is asked
    /// again, the safe direction.
    fn persist(&self, new_key: &GrantKey) {
        let Some(persisted) = &self.persisted else {
            return;
        };
        match persisted.add(&new_key.0, &new_key.1) {
            Ok(all) => {
                let mut g = self.inner.lock().expect("extra-roots mutex poisoned");
                g.always.extend(all);
            }
            Err(e) => tracing::warn!("could not persist an extra-roots grant: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #524: a trusted scratch dir is durably allowed for any tool, with no
    /// prior grant of any kind.
    #[test]
    fn scratch_dir_is_durably_allowed_with_no_grant() {
        let dir = tempfile::tempdir().unwrap();
        let s = ExtraRootStore::ephemeral().with_scratch(dir.path().to_path_buf());
        assert!(s.is_durably_allowed("read", dir.path()));
        assert!(s.is_durably_allowed("write", dir.path()));
        assert!(s.is_durably_allowed("bash", dir.path()));
    }

    /// #524: the carve-out covers descendants of the scratch dir too, not just
    /// the exact directory.
    #[test]
    fn scratch_dir_carve_out_covers_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let s = ExtraRootStore::ephemeral().with_scratch(dir.path().to_path_buf());
        assert!(s.is_durably_allowed("write", &nested.join("out.txt")));
        assert!(s.take_allowance("write", &nested.join("out.txt"), "req-1"));
    }

    /// #524: a sibling directory outside the scratch dir is unaffected — the
    /// carve-out doesn't widen to the scratch dir's parent.
    #[test]
    fn scratch_dir_carve_out_does_not_cover_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        let sibling = dir.path().join("sibling");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let s = ExtraRootStore::ephemeral().with_scratch(scratch);
        assert!(!s.is_durably_allowed("write", &sibling));
        assert!(!s.take_allowance("write", &sibling, "req-1"));
    }

    /// #524: `take_allowance` under the scratch dir never consumes a `Once`
    /// token — there's nothing to spend, it's just always allowed.
    #[test]
    fn scratch_dir_take_allowance_does_not_consume_once_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let s = ExtraRootStore::ephemeral().with_scratch(dir.path().to_path_buf());
        let target = dir.path().join("file.txt");
        assert!(s.take_allowance("read", &target, "req-1"));
        assert!(s.take_allowance("read", &target, "req-2"), "repeatable");
    }

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

    /// A store over an `AlwaysRoots` loads its grants and records new
    /// `Always` ones there — and only those.
    #[test]
    fn always_grants_load_from_and_record_into_the_seam() {
        #[derive(Default)]
        struct Memory(Mutex<Vec<(String, String)>>);
        impl AlwaysRoots for Memory {
            fn load(&self) -> Vec<(String, String)> {
                self.0.lock().unwrap().clone()
            }
            fn add(&self, tool: &str, path: &str) -> anyhow::Result<Vec<(String, String)>> {
                let mut g = self.0.lock().unwrap();
                g.push((tool.into(), path.into()));
                Ok(g.clone())
            }
        }
        let seam = Arc::new(Memory::default());
        seam.add("read", "/opt/old").unwrap();

        let s = ExtraRootStore::ephemeral().with_always_roots(seam.clone());
        assert!(s.is_durably_allowed("read", Path::new("/opt/old")));
        s.record("read", Path::new("/opt/new"), ApprovalScope::Always, "r1");
        s.record("read", Path::new("/opt/tmp"), ApprovalScope::Session, "r2");
        assert_eq!(seam.load().len(), 2, "only Always reaches the seam");

        let fresh = ExtraRootStore::ephemeral().with_always_roots(seam);
        assert!(fresh.is_durably_allowed("read", Path::new("/opt/new")));
        assert!(!fresh.is_durably_allowed("read", Path::new("/opt/tmp")));
    }
}
