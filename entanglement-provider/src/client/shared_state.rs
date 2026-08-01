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
//! [`DISABLE_ENV`] — instances just don't coordinate, they don't break.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::time::sleep;

use super::shared_store::{self, Admission, SharedState};

/// How often a held lease is refreshed while its request is still in flight.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(60);

/// Opt out of cross-process sharing entirely and revert to pure in-process
/// gating (today's pre-#523 behavior) — e.g. an operator running unrelated
/// workloads under the same provider key on purpose who wants neither to
/// throttle the other. Giving each instance its own key/base URL already
/// isolates them for free (the existing pool-key partitioning, #217/#523);
/// this flag is for the same-key case.
const DISABLE_ENV: &str = "ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE";

/// Override the directory shared endpoint state files live under. Mainly for
/// tests; an operator could also point it somewhere durable across a
/// container's ephemeral `$XDG_DATA_HOME`.
const STATE_DIR_ENV: &str = "ENTANGLEMENT_SHARED_STATE_DIR";

fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    dirs::data_dir().map(|d| d.join("entanglement").join("endpoints"))
}

/// One endpoint's cross-process gate. Cheap to hold — the real state lives in
/// the file at `path`, re-read and rewritten under lock on every admission
/// attempt; this struct is just the resolved identity. `path` is `None` when
/// sharing is disabled or the data directory can't be determined, in which
/// case [`acquire`][Self::acquire] is a no-op that returns `None` and the
/// caller falls back to in-process-only gating.
pub(crate) struct SharedGate {
    path: Option<PathBuf>,
    pid: u32,
}

impl SharedGate {
    /// Resolve (but don't yet touch) the shared-state file for pool key
    /// `key` — the same `(base URL, API-key hash)` identity the in-process
    /// pool keys on. `None` when disabled via [`DISABLE_ENV`] or the data
    /// directory can't be determined; both degrade to in-process-only
    /// gating.
    pub(crate) fn new(key: &str) -> Self {
        let path = if std::env::var(DISABLE_ENV).as_deref() == Ok("1") {
            None
        } else {
            state_dir().map(|dir| dir.join(format!("{}.state", shared_store::hash_key(key))))
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

    fn tmp_state_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("endpoint.state");
        (dir, path)
    }

    #[test]
    fn env_disable_yields_no_shared_gate() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(DISABLE_ENV, "1");
        let gate = SharedGate::new("https://api.example/v1#deadbeef");
        std::env::remove_var(DISABLE_ENV);
        assert!(gate.path.is_none());
    }

    #[tokio::test]
    async fn acquire_end_to_end_admits_then_waits_then_releases() {
        let (_dir, path) = tmp_state_path();
        std::env::set_var(STATE_DIR_ENV, path.parent().unwrap());
        let gate = SharedGate {
            path: Some(path),
            pid: std::process::id(),
        };
        std::env::remove_var(STATE_DIR_ENV);

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
            tokio::time::timeout(Duration::from_secs(2), gate.acquire(100, 1, deadline)).await;
        assert!(third.is_ok(), "slot must be released once the lease drops");
    }

    #[tokio::test]
    async fn acquire_gives_up_once_the_deadline_is_exceeded() {
        // #547: a caller must not poll the shared gate forever — a saturated
        // cap (or a persisted cool-down from a sibling/previous run) that
        // outlives the caller's own `rate_limit_max_elapsed` budget must
        // surface as `Err(())` instead of hanging.
        let (_dir, path) = tmp_state_path();
        std::env::set_var(STATE_DIR_ENV, path.parent().unwrap());
        let gate = SharedGate {
            path: Some(path),
            pid: std::process::id(),
        };
        std::env::remove_var(STATE_DIR_ENV);

        // Cap of 1, held by this "instance" itself, so a second acquire can
        // never be admitted — it must instead give up at `deadline`.
        let _held = gate
            .acquire(100, 1, Instant::now() + Duration::from_secs(5))
            .await
            .expect("deadline generous enough not to be hit")
            .expect("shared state dir is writable in this test");

        let deadline = Instant::now() + Duration::from_millis(100);
        let result = tokio::time::timeout(Duration::from_secs(2), gate.acquire(100, 1, deadline))
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
