//! Cross-process shared endpoint resilience state (#523, ADR-0144).
//!
//! Multiple `skutter` processes talking to the same `(base URL, API-key)`
//! endpoint used to each run a fully independent in-process pool
//! (`EndpointState`, ADR-0050/ADR-0111/ADR-0122) — N processes each believed
//! they owned the whole RPM/concurrency budget, so together they could send
//! up to N× the configured rate and hold N× the configured concurrency
//! against a provider that has no idea it's talking to more than one client.
//!
//! This module makes the RPM budget, the in-flight concurrency count, and the
//! 429 `Retry-After` cool-down **cross-process facts**, file-backed at
//! `${data_dir}/entanglement/endpoints/<sha256(pool_key)>.state`. The on-disk
//! format and the locked read-modify-write mechanics live in
//! [`super::shared_store`] (split out purely to keep both files under the
//! 400-line cap, #552); this module owns the public API and admission
//! policy.
//!
//! The AIMD pacing gate (`RateLimiter`, ADR-0111) stays **per-process** (v1) —
//! see ADR-0144 for the full reasoning and rejected alternatives (a broker
//! daemon, static partitioning).
//!
//! Concurrency is **lease-based**, not a shared semaphore: each admitted
//! request writes a lease (an id + owning pid + expiry) and renews it on a
//! heartbeat while the request is in flight. A process that dies (killed,
//! panicked) simply stops renewing; the next process to touch the file prunes
//! the lease once its TTL elapses, so slots are recovered rather than leaked
//! permanently. Every caller also releases its own lease **synchronously** on
//! drop (#547) — the TTL is the backstop for the unrecoverable case (a
//! `SIGKILL`), not the normal path.
//!
//! Falls back silently to pure in-process behavior (pre-#523) when the state
//! directory can't be created or written, or when explicitly disabled via
//! `ENTANGLEMENT_DEBUG=no-shared-state` — instances just don't coordinate,
//! they don't break.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::time::sleep;

use super::shared_store::{self, Admission, SharedState};

/// How often a held lease is refreshed while its request is still in flight.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(60);

/// Override the directory shared endpoint state files live under — an
/// undocumented hook for subprocess tests since ADR-0213 (an in-process
/// caller injects a [`SharedStateLocation`] instead, #743).
const STATE_DIR_ENV: &str = "ENTANGLEMENT_SHARED_STATE_DIR";

/// Where an [`HttpClient`](super::HttpClient)'s cross-process endpoint state
/// lives — injected per client (#743) rather than read from the process env
/// at every endpoint creation, so a test (or an embedder running several
/// isolated pools in one process) can pin its own directory, or opt out,
/// without mutating process-global env that parallel callers also read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SharedStateLocation {
    /// No cross-process coordination: pure in-process gating (pre-#523).
    Disabled,
    /// `.state`/`.lock` files live directly under this directory.
    Dir(PathBuf),
}

impl SharedStateLocation {
    /// The production resolution every env-driven constructor uses:
    /// `ENTANGLEMENT_DEBUG=no-shared-state` > `ENTANGLEMENT_SHARED_STATE_DIR`
    /// > `${data_dir}/entanglement/endpoints` > disabled (no data dir).
    ///
    /// The opt-out exists for an operator running unrelated workloads under
    /// the same provider key on purpose who wants neither to throttle the
    /// other — a distinct key/base URL already isolates them (#217/#523).
    pub fn from_env() -> Self {
        Self::resolve(|name| std::env::var_os(name), default_state_dir)
    }

    /// [`from_env`][Self::from_env] over an injected env lookup and data-dir
    /// fallback, so the precedence is unit-testable without `set_var`.
    fn resolve(
        env: impl Fn(&str) -> Option<OsString>,
        default_dir: impl FnOnce() -> Option<PathBuf>,
    ) -> Self {
        if crate::debug::DebugFlags::from_lookup(&env).no_shared_state {
            return Self::Disabled;
        }
        env(STATE_DIR_ENV)
            .map(PathBuf::from)
            .or_else(default_dir)
            .map_or(Self::Disabled, Self::Dir)
    }

    /// Sweep this location for idle, lease-free `.state`/`.lock` pairs — see
    /// [`prune_stale`]. A no-op when disabled. Returns the pairs removed.
    pub fn prune_stale(&self, max_idle: Duration) -> usize {
        match self {
            Self::Disabled => 0,
            Self::Dir(dir) => shared_store::prune_orphaned(dir, max_idle),
        }
    }
}

fn default_state_dir() -> Option<PathBuf> {
    // Guard (#743): a unit test reaching the real data dir would read and
    // write the developer's live endpoint state and race every other test
    // doing the same — fail it loudly so the test injects a location instead.
    // `cfg!` (not `#[cfg]` items) keeps this out of the file-cap gate's
    // first-`#[cfg(test)]`-marker cut-off.
    if cfg!(test) {
        panic!(
            "test resolved the real shared endpoint-state dir; build the client with \
             HttpClient::with_shared_state and an explicit SharedStateLocation"
        );
    }
    dirs::data_dir().map(|d| d.join("entanglement").join("endpoints"))
}

/// One endpoint's cross-process gate. Cheap to hold — the real state lives in
/// the file at `path`, re-read and rewritten under lock on every admission
/// attempt; this struct is just the resolved identity. `path` is `None` when
/// sharing is disabled, in which case [`acquire`][Self::acquire] is a no-op
/// that returns `None` and the caller falls back to in-process-only gating.
pub(crate) struct SharedGate {
    path: Option<PathBuf>,
    pid: u32,
}

impl SharedGate {
    /// Resolve (but don't yet touch) the shared-state file for pool key
    /// `key` under `location` — the same `(base URL, API-key hash)` identity
    /// the in-process pool keys on.
    pub(crate) fn new(key: &str, location: &SharedStateLocation) -> Self {
        let path = match location {
            SharedStateLocation::Disabled => None,
            SharedStateLocation::Dir(dir) => {
                Some(dir.join(format!("{}.state", shared_store::hash_key(key))))
            }
        };
        Self {
            path,
            pid: std::process::id(),
        }
    }

    /// Block until admitted under the shared RPM budget, concurrency cap, and
    /// `retry_after` cool-down, then return a lease the caller must hold for
    /// the whole request plus its streamed body. `Ok(None)` means sharing is
    /// disabled/unwritable — fall back to in-process gates. `Err(())` means
    /// waiting further would extend past `deadline`, the caller's own
    /// `rate_limit_max_elapsed` budget (#547): a cool-down read back from the
    /// shared file must not park a caller past its own budget.
    pub(crate) async fn acquire(
        &self,
        rpm: u32,
        concurrency: usize,
        deadline: Instant,
    ) -> Result<Option<SharedLease>, ()> {
        let Some(path) = self.path.clone() else {
            return Ok(None);
        };
        loop {
            if Instant::now() >= deadline {
                return Err(());
            }
            let (p, pid) = (path.clone(), self.pid);
            let result = tokio::task::spawn_blocking(move || {
                shared_store::try_admit(&p, rpm, concurrency, pid)
            })
            .await
            .unwrap_or(Err(()));
            match result {
                Ok(Admission::Admitted(lease_id)) => {
                    return Ok(Some(SharedLease::spawn(path, lease_id)))
                }
                Ok(Admission::Wait(dur)) => {
                    let wait = dur.max(Duration::from_millis(10));
                    if Instant::now() + wait >= deadline {
                        return Err(());
                    }
                    sleep(wait).await;
                }
                Err(()) => {
                    tracing::debug!(
                        path = %path.display(),
                        "shared endpoint state unwritable; falling back to in-process gating"
                    );
                    return Ok(None);
                }
            }
        }
    }

    /// Record a 429's cool-down in the shared file too, so a sibling
    /// instance's next [`acquire`][Self::acquire] parks until the same
    /// deadline instead of immediately re-saturating the endpoint. Best-effort
    /// (a failed write is discovered by the sibling on its own next 429,
    /// as before #523); awaited rather than fired off as a detached
    /// `tokio::spawn` (#547), which a short-lived one-shot `run` could exit
    /// past before it ever ran.
    pub(crate) async fn mark_retry_after(&self, delay: Duration) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let _ =
            tokio::task::spawn_blocking(move || shared_store::set_shared_retry_after(&path, delay))
                .await;
    }

    /// A best-effort, **lock-free** read of this endpoint's shared file, for
    /// status display only (#552) — a peer process's parked 429, or a lease
    /// it holds (live or merely not-yet-pruned), used to read as "at rest"
    /// until *this* process's own request happened to touch the shared file.
    /// Skips the advisory lock entirely: the file is always replaced via an
    /// atomic rename (see `shared_store::with_locked_state`), so a concurrent
    /// writer can never be observed mid-write — only up to one write cycle
    /// stale, which is fine for a status label, unlike
    /// [`acquire`][Self::acquire]'s admission decision. Synchronous (a single
    /// small local file read) so callers on the render path (the TUI's
    /// per-frame `throttle_status`) don't need to hop to a blocking pool for
    /// it. `None` when sharing is disabled, the file doesn't exist yet, or it
    /// fails to parse.
    pub(crate) fn peek(&self) -> Option<SharedSnapshot> {
        let path = self.path.as_ref()?;
        let bytes = fs::read(path).ok()?;
        let state: SharedState = serde_json::from_slice(&bytes).ok()?;
        let now = shared_store::now_ms();
        let cool_down_remaining = state
            .retry_after_until_ms
            .filter(|&until| until > now)
            .map(|until| Duration::from_millis(until - now));
        let leases = state
            .leases
            .iter()
            .filter(|lease| lease.expires_at_ms > now)
            .count();
        Some(SharedSnapshot {
            cool_down_remaining,
            leases,
        })
    }
}

/// Sweep the env-resolved shared-state directory
/// ([`SharedStateLocation::from_env`]) for `.state`/`.lock` pairs that have
/// gone idle for at least `max_idle` *and* carry no live lease, pending
/// cool-down, or recent request (#551) — the artifact of a `/key` rotation, a
/// catalog `base_url` edit, or a decommissioned provider, none of which ever
/// evict the shared-state file their old pool key wrote. Intended as a
/// best-effort startup sweep (mirrors `session_store::prune`'s role for
/// session logs); a no-op when sharing is disabled or the state directory
/// can't be determined. Returns the number of pairs removed.
pub fn prune_stale(max_idle: Duration) -> usize {
    SharedStateLocation::from_env().prune_stale(max_idle)
}

/// A point-in-time, cross-process view of one endpoint's shared gate — see
/// [`SharedGate::peek`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SharedSnapshot {
    /// Remaining time on a cool-down *any* process sharing this endpoint set
    /// — including one this process never saw the 429 for itself.
    pub(crate) cool_down_remaining: Option<Duration>,
    /// Count of currently-live leases across every process sharing this
    /// endpoint (this process's own included, if it holds any).
    pub(crate) leases: usize,
}

/// A held cross-process concurrency slot. Renewed on a background heartbeat
/// while alive; releasing it removes the lease **synchronously in `Drop`**
/// (#547), not via a detached task — a short-lived one-shot `run` (or a
/// SIGINT/SIGTERM shutdown) can tear the tokio runtime down before a detached
/// cleanup task ever gets scheduled.
pub(crate) struct SharedLease {
    path: PathBuf,
    lease_id: u64,
    cancel: Option<oneshot::Sender<()>>,
}

impl SharedLease {
    fn spawn(path: PathBuf, lease_id: u64) -> Self {
        let (tx, mut rx) = oneshot::channel();
        let renew_path = path.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    _ = sleep(LEASE_RENEW_INTERVAL) => {
                        let p = renew_path.clone();
                        let _ = tokio::task::spawn_blocking(move || shared_store::renew_lease(&p, lease_id)).await;
                    }
                }
            }
        });
        Self {
            path,
            lease_id,
            cancel: Some(tx),
        }
    }
}

impl Drop for SharedLease {
    fn drop(&mut self) {
        // Cancel the renewal heartbeat first so it can't resurrect the lease.
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
        let _ = shared_store::remove_lease(&self.path, self.lease_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::DEBUG_ENV;

    fn tmp_state_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("endpoint.state");
        (dir, path)
    }

    /// An env lookup over a fixed table — the injected stand-in for
    /// `std::env::var_os`, so precedence tests never touch the process env.
    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn data_dir() -> Option<PathBuf> {
        Some(PathBuf::from("/data/entanglement/endpoints"))
    }

    #[test]
    fn resolve_defaults_to_the_data_dir() {
        let loc = SharedStateLocation::resolve(env_of(&[]), data_dir);
        assert_eq!(loc, SharedStateLocation::Dir(data_dir().unwrap()));
    }

    #[test]
    fn resolve_prefers_the_dir_override_over_the_data_dir() {
        let loc = SharedStateLocation::resolve(env_of(&[(STATE_DIR_ENV, "/tmp/x")]), data_dir);
        assert_eq!(loc, SharedStateLocation::Dir(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn resolve_disable_wins_over_the_dir_override() {
        let env = env_of(&[
            (DEBUG_ENV, "bodies,no-shared-state"),
            (STATE_DIR_ENV, "/tmp/x"),
        ]);
        let loc = SharedStateLocation::resolve(env, || panic!("must not consult the data dir"));
        assert_eq!(loc, SharedStateLocation::Disabled);
    }

    #[test]
    fn resolve_ignores_debug_tokens_other_than_no_shared_state() {
        let loc = SharedStateLocation::resolve(env_of(&[(DEBUG_ENV, "bodies,echo")]), data_dir);
        assert_eq!(loc, SharedStateLocation::Dir(data_dir().unwrap()));
    }

    #[test]
    fn resolve_is_disabled_without_any_data_dir() {
        let loc = SharedStateLocation::resolve(env_of(&[]), || None);
        assert_eq!(loc, SharedStateLocation::Disabled);
    }

    #[test]
    #[should_panic(expected = "real shared endpoint-state dir")]
    fn unit_tests_may_not_resolve_the_real_data_dir() {
        let _ = SharedStateLocation::resolve(env_of(&[]), default_state_dir);
    }

    #[test]
    fn disabled_location_yields_no_shared_gate() {
        let gate = SharedGate::new(
            "https://api.example/v1#deadbeef",
            &SharedStateLocation::Disabled,
        );
        assert!(gate.path.is_none());
    }

    #[test]
    fn prune_stale_sweeps_the_location_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale_path = dir
            .path()
            .join(format!("{}.state", shared_store::hash_key("k")));
        fs::write(
            &stale_path,
            serde_json::to_vec(&SharedState::default()).unwrap(),
        )
        .unwrap();
        fs::File::open(&stale_path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200))
            .unwrap();

        let location = SharedStateLocation::Dir(dir.path().to_path_buf());
        let removed = location.prune_stale(Duration::from_secs(3600));

        assert_eq!(removed, 1);
        assert!(!stale_path.exists());
    }

    #[test]
    fn prune_stale_is_a_noop_when_sharing_is_disabled() {
        let removed = SharedStateLocation::Disabled.prune_stale(Duration::from_secs(0));
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn acquire_end_to_end_admits_then_waits_then_releases() {
        let (_dir, path) = tmp_state_path();
        let gate = gate_at(path);

        let deadline = Instant::now() + Duration::from_secs(5);
        let lease = gate
            .acquire(100, 1, deadline)
            .await
            .expect("deadline is generous enough not to be hit")
            .expect("shared state dir is writable in this test");
        // Cap is 1 and the lease above holds it — a second acquire attempt
        // from the "same instance" must not be immediately admitted.
        let second =
            tokio::time::timeout(Duration::from_millis(50), gate.acquire(100, 1, deadline)).await;
        assert!(
            second.is_err(),
            "expected the second acquire to still be waiting"
        );
        // Release is synchronous in `Drop` (#547) — no need to wait for a
        // detached task to catch up before the slot is visibly free.
        drop(lease);
        let third =
            tokio::time::timeout(Duration::from_secs(10), gate.acquire(100, 1, deadline)).await;
        assert!(third.is_ok(), "slot must be released once the lease drops");
    }

    #[tokio::test]
    async fn acquire_gives_up_once_the_deadline_is_exceeded() {
        // #547: a caller must not poll the shared gate forever — a saturated
        // cap (or a persisted cool-down from a sibling/previous run) that
        // outlives the caller's own `rate_limit_max_elapsed` budget must
        // surface as `Err(())` instead of hanging.
        let (_dir, path) = tmp_state_path();
        let gate = gate_at(path);

        // Cap of 1, held by this "instance" itself, so a second acquire can
        // never be admitted — it must instead give up at `deadline`.
        let _held = gate
            .acquire(100, 1, Instant::now() + Duration::from_secs(5))
            .await
            .expect("deadline generous enough not to be hit")
            .expect("shared state dir is writable in this test");

        let deadline = Instant::now() + Duration::from_millis(100);
        let result = tokio::time::timeout(Duration::from_secs(10), gate.acquire(100, 1, deadline))
            .await
            .expect("must give up at the deadline instead of hanging");
        assert!(matches!(result, Err(())));
    }

    fn gate_at(path: PathBuf) -> SharedGate {
        SharedGate {
            path: Some(path),
            pid: std::process::id(),
        }
    }

    #[test]
    fn peek_reports_a_peers_cool_down_this_process_never_saw() {
        // #552: instance A's 429 never touched this process directly — only
        // the shared file. `peek` must still surface it so the status label
        // doesn't read "at rest" while a sibling is parked.
        let (_dir, path) = tmp_state_path();
        shared_store::set_shared_retry_after(&path, Duration::from_secs(30))
            .expect("set retry-after");
        let snapshot = gate_at(path).peek().expect("file exists and parses");
        let remaining = snapshot
            .cool_down_remaining
            .expect("cool-down surfaced from the shared file alone");
        assert!(
            remaining > Duration::from_secs(25) && remaining <= Duration::from_secs(30),
            "got {remaining:?}"
        );
    }

    #[test]
    fn peek_reports_live_lease_count() {
        let (_dir, path) = tmp_state_path();
        let state = SharedState {
            request_times_ms: Vec::new(),
            retry_after_until_ms: None,
            leases: vec![
                shared_store::Lease {
                    id: 1,
                    pid: 111,
                    expires_at_ms: shared_store::now_ms() + 60_000,
                },
                shared_store::Lease {
                    id: 2,
                    pid: 222,
                    expires_at_ms: shared_store::now_ms() + 60_000,
                },
            ],
        };
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        let snapshot = gate_at(path).peek().expect("file exists and parses");
        assert_eq!(snapshot.leases, 2);
        assert!(snapshot.cool_down_remaining.is_none());
    }

    #[test]
    fn peek_excludes_expired_leases_and_a_cleared_cool_down() {
        let (_dir, path) = tmp_state_path();
        let state = SharedState {
            request_times_ms: Vec::new(),
            retry_after_until_ms: Some(shared_store::now_ms().saturating_sub(1_000)),
            leases: vec![shared_store::Lease {
                id: 1,
                pid: 111,
                expires_at_ms: shared_store::now_ms().saturating_sub(1_000),
            }],
        };
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        let snapshot = gate_at(path).peek().expect("file exists and parses");
        assert_eq!(snapshot.leases, 0);
        assert!(snapshot.cool_down_remaining.is_none());
    }

    #[test]
    fn peek_is_none_when_sharing_disabled_or_file_missing() {
        assert!(SharedGate { path: None, pid: 1 }.peek().is_none());
        let (_dir, path) = tmp_state_path();
        // `path` was never written to — no such file yet.
        assert!(gate_at(path).peek().is_none());
    }
}
