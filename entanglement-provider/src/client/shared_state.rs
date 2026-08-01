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
//! `${data_dir}/entanglement/endpoints/<sha256(pool_key)>.state`
//! ([`state_path`]), guarded by the same advisory `fd-lock` read-modify-write
//! pattern the managed config files use (#329, ADR-0084;
//! [`with_locked_state`] mirrors `entanglement-runtime::config::lock::with_locked_file`,
//! independently re-implemented rather than depended on since
//! `entanglement-provider` is the leaf crate and takes no `entanglement-*`
//! dependency, ADR-0053).
//!
//! The AIMD pacing gate (`RateLimiter`, ADR-0111) stays **per-process** (v1):
//! once the RPM budget and concurrency cap are themselves shared and
//! correctly bounded in aggregate, per-process pacing is smoothing a signal
//! that's already correct — see ADR-0144 for the full reasoning and the
//! rejected alternatives (a broker daemon, static partitioning).
//!
//! Concurrency is **lease-based**, not a shared semaphore: each admitted
//! request writes a lease (an id + owning pid + expiry) and renews it on a
//! heartbeat while the request is in flight. A process that dies (killed,
//! panicked) simply stops renewing; the next process to touch the file prunes
//! the lease once its TTL elapses, so slots are recovered rather than leaked
//! permanently, with no explicit peer-liveness detection needed.
//!
//! Falls back silently to pure in-process behavior (today's pre-#523
//! behavior) when the state directory can't be created or written
//! (permissions, a read-only filesystem, a sandboxed test environment) or
//! when explicitly disabled via [`DISABLE_ENV`] — instances just don't
//! coordinate, they don't break.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::time::sleep;

/// How long an unrenewed concurrency lease is honored before it's treated as
/// abandoned (a crashed/killed process) and its slot recovered. Comfortably
/// above [`LEASE_RENEW_INTERVAL`] so a live holder never lapses under normal
/// scheduling jitter.
const LEASE_TTL: Duration = Duration::from_secs(180);

/// How often a held lease is refreshed while its request is still in flight.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(60);

/// Polling cadence while waiting on the shared RPM budget or concurrency cap
/// to free up. This is local sleep-then-retry, not a blocking wait — a
/// deliberately short interval costs one cheap locked file read/write per
/// tick, not a busy loop, and keeps a freed slot from sitting unnoticed long
/// after a shared `Retry-After` deadline or RPM window passes.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

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

/// What a single locked read-modify-write pass decided.
#[derive(Debug)]
enum Admission {
    /// Admitted; holds the lease id to renew/release.
    Admitted(u64),
    /// Not admitted yet; retry after (at least) this long.
    Wait(Duration),
}

/// The file's on-disk shape. Deliberately minimal — a RPM ledger, live
/// leases, and a shared cool-down deadline are the whole cross-process
/// contract (ADR-0144).
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct SharedState {
    #[serde(default)]
    request_times_ms: Vec<u64>,
    #[serde(default)]
    retry_after_until_ms: Option<u64>,
    #[serde(default)]
    leases: Vec<Lease>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Lease {
    id: u64,
    pid: u32,
    expires_at_ms: u64,
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
            state_dir().map(|dir| dir.join(format!("{}.state", hash_key(key))))
        };
        Self {
            path,
            pid: std::process::id(),
        }
    }

    /// Block until admitted under the shared RPM budget, concurrency cap, and
    /// `retry_after` cool-down, then return a lease the caller must hold for
    /// the whole request plus its streamed body. Returns `None` immediately
    /// when sharing is disabled or the state directory proved unwritable —
    /// the caller is unaffected, having already applied its own in-process
    /// gates for the same `rpm`/`concurrency`.
    pub(crate) async fn acquire(&self, rpm: u32, concurrency: usize) -> Option<SharedLease> {
        let path = self.path.clone()?;
        loop {
            let p = path.clone();
            let pid = self.pid;
            let result = tokio::task::spawn_blocking(move || try_admit(&p, rpm, concurrency, pid))
                .await
                .unwrap_or(Err(()));
            match result {
                Ok(Admission::Admitted(lease_id)) => {
                    return Some(SharedLease::spawn(path, lease_id))
                }
                Ok(Admission::Wait(dur)) => sleep(dur.max(Duration::from_millis(10))).await,
                Err(()) => {
                    tracing::debug!(
                        path = %path.display(),
                        "shared endpoint state unwritable; falling back to in-process gating"
                    );
                    return None;
                }
            }
        }
    }

    /// Record a 429's cool-down in the shared file too, so a sibling
    /// instance's next [`acquire`][Self::acquire] parks until the same
    /// deadline instead of immediately re-saturating the endpoint the moment
    /// this process backs off. Best-effort and fire-and-forget: a failed
    /// write just means the sibling discovers the cool-down itself on its own
    /// next 429, exactly as before #523.
    pub(crate) fn mark_retry_after(&self, delay: Duration) {
        let Some(path) = self.path.clone() else {
            return;
        };
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || set_shared_retry_after(&path, delay)).await;
        });
    }
}

/// A held cross-process concurrency slot. Renewed on a background heartbeat
/// while alive; releasing it (drop) cancels the heartbeat and removes the
/// lease. If the process is killed instead of dropping this cleanly, the
/// lease simply stops being renewed and a later admission attempt (by any
/// process sharing the file) prunes it once [`LEASE_TTL`] elapses.
pub(crate) struct SharedLease {
    cancel: Option<oneshot::Sender<()>>,
}

impl SharedLease {
    fn spawn(path: PathBuf, lease_id: u64) -> Self {
        let (tx, mut rx) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    _ = sleep(LEASE_RENEW_INTERVAL) => {
                        let p = path.clone();
                        let _ = tokio::task::spawn_blocking(move || renew_lease(&p, lease_id)).await;
                    }
                }
            }
            let _ = tokio::task::spawn_blocking(move || remove_lease(&path, lease_id)).await;
        });
        Self { cancel: Some(tx) }
    }
}

impl Drop for SharedLease {
    fn drop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }
}

/// A single locked read-modify-write attempt: prune stale entries, then
/// admit if the shared cool-down has cleared and both the RPM ledger and the
/// lease count have room. Combines all three checks into one lock
/// acquisition so a caller need not round-trip the file three times per
/// attempt.
fn try_admit(path: &Path, rpm: u32, concurrency: usize, pid: u32) -> Result<Admission, ()> {
    with_locked_state(path, |state| {
        let now = now_ms();
        prune(state, now);

        if let Some(until) = state.retry_after_until_ms {
            if until > now {
                return Admission::Wait(Duration::from_millis(until - now));
            }
        }

        let cap = concurrency.max(1);
        if state.leases.len() >= cap {
            return Admission::Wait(POLL_INTERVAL);
        }

        let rpm = rpm.max(1) as usize;
        if state.request_times_ms.len() >= rpm {
            let oldest = state.request_times_ms[0];
            let wait_ms = (oldest + 60_000).saturating_sub(now).max(1);
            return Admission::Wait(Duration::from_millis(wait_ms));
        }

        let lease_id = next_lease_id();
        state.request_times_ms.push(now);
        state.leases.push(Lease {
            id: lease_id,
            pid,
            expires_at_ms: now + LEASE_TTL.as_millis() as u64,
        });
        Admission::Admitted(lease_id)
    })
}

fn renew_lease(path: &Path, lease_id: u64) -> Result<(), ()> {
    with_locked_state(path, |state| {
        let now = now_ms();
        for lease in state.leases.iter_mut() {
            if lease.id == lease_id {
                lease.expires_at_ms = now + LEASE_TTL.as_millis() as u64;
            }
        }
    })
}

fn remove_lease(path: &Path, lease_id: u64) -> Result<(), ()> {
    with_locked_state(path, |state| {
        state.leases.retain(|lease| lease.id != lease_id);
    })
}

fn set_shared_retry_after(path: &Path, delay: Duration) -> Result<(), ()> {
    with_locked_state(path, |state| {
        // Saturating throughout: `delay` is caller-supplied and, pre-#548,
        // an unclamped huge `Retry-After` could truncate through the
        // u128->u64 millis cast and/or overflow the add, panicking in debug
        // builds. `parse_retry_after` now clamps at the source, but this
        // stays saturating too — the last line of defense for any other
        // caller of `mark_retry_after`.
        let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        let until = now_ms().saturating_add(delay_ms);
        state.retry_after_until_ms = Some(match state.retry_after_until_ms {
            Some(existing) if existing > until => existing,
            _ => until,
        });
    })
}

/// Drop leases past their TTL and request timestamps outside the trailing
/// 60s RPM window; clear an elapsed cool-down. Run at the top of every locked
/// access so a stale entry never survives past the next process to touch the
/// file, regardless of which operation that is.
fn prune(state: &mut SharedState, now: u64) {
    state.leases.retain(|lease| lease.expires_at_ms > now);
    state
        .request_times_ms
        .retain(|&t| now.saturating_sub(t) < 60_000);
    if state.retry_after_until_ms.is_some_and(|until| until <= now) {
        state.retry_after_until_ms = None;
    }
}

/// Run `f` over the current on-disk state under an exclusive advisory lock on
/// `path`'s `.lock` sibling, then persist whatever `f` mutated. Creates the
/// parent directory on first use. `Err(())` covers every filesystem failure
/// (unwritable directory, permissions, a read-only filesystem) uniformly —
/// the caller's only reaction is "fall back to in-process gating," so the
/// specific cause isn't threaded through.
fn with_locked_state<T>(path: &Path, f: impl FnOnce(&mut SharedState) -> T) -> Result<T, ()> {
    let dir = path.parent().ok_or(())?;
    fs::create_dir_all(dir).map_err(|_| ())?;
    let lock_path = path.with_extension("lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| ())?;
    let mut rw_lock = fd_lock::RwLock::new(lock_file);
    let _guard = rw_lock.write().map_err(|_| ())?;

    let mut state = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let result = f(&mut state);
    let bytes = serde_json::to_vec(&state).map_err(|_| ())?;
    let tmp_path = path.with_extension("state.tmp");
    fs::write(&tmp_path, bytes).map_err(|_| ())?;
    fs::rename(&tmp_path, path).map_err(|_| ())?;
    Ok(result)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A process-unique-enough lease id: no cryptographic quality needed (mirrors
/// `client::jitter_unit`'s own no-`rand`-dependency reasoning), just low
/// collision odds between the handful of processes that might share one
/// endpoint file. Combines the pid (distinguishes processes), a monotonic
/// per-process counter (distinguishes leases from the same process), and the
/// wall clock's subsecond nanos (spreads restarts of the same pid apart).
static LEASE_COUNTER: AtomicU64 = AtomicU64::new(0);
fn next_lease_id() -> u64 {
    let counter = LEASE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    ((std::process::id() as u64) << 40) ^ (nanos << 8) ^ counter
}

/// Hash a pool key to a filesystem-safe name — the key already carries the
/// API-key hash suffix (`client::pool_key`), so this is a second, structural
/// hash purely so the state file name is never a raw URL.
fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
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
    fn combined_rpm_ceiling_across_two_simulated_instances() {
        // rpm: 2 shared across two "instances" hitting the same file — the
        // third request (regardless of which instance issues it) must not be
        // admitted until the RPM window has room.
        let (_dir, path) = tmp_state_path();
        assert!(matches!(
            try_admit(&path, 2, 100, 111),
            Ok(Admission::Admitted(_))
        ));
        assert!(matches!(
            try_admit(&path, 2, 100, 222),
            Ok(Admission::Admitted(_))
        ));
        assert!(matches!(
            try_admit(&path, 2, 100, 333),
            Ok(Admission::Wait(_))
        ));
    }

    #[test]
    fn combined_concurrency_cap_across_two_simulated_instances() {
        // concurrency: 2, rpm generous — two leases fill the cap regardless
        // of which pid holds them; a third must wait even though the RPM
        // budget alone would allow it.
        let (_dir, path) = tmp_state_path();
        assert!(matches!(
            try_admit(&path, 100, 2, 111),
            Ok(Admission::Admitted(_))
        ));
        assert!(matches!(
            try_admit(&path, 100, 2, 222),
            Ok(Admission::Admitted(_))
        ));
        assert!(matches!(
            try_admit(&path, 100, 2, 333),
            Ok(Admission::Wait(_))
        ));
    }

    #[test]
    fn shared_retry_after_parks_a_different_instance() {
        // Instance A (pid 111) sees a 429 and records the cool-down; instance
        // B (pid 222), which never saw the 429 itself, must still be parked
        // by the shared deadline on its very next admission attempt.
        let (_dir, path) = tmp_state_path();
        set_shared_retry_after(&path, Duration::from_secs(30)).expect("set retry-after");
        match try_admit(&path, 100, 100, 222) {
            Ok(Admission::Wait(dur)) => {
                assert!(dur > Duration::from_secs(25) && dur <= Duration::from_secs(30));
            }
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn shared_retry_after_saturates_instead_of_panicking_on_huge_delay() {
        // A huge `delay` (pre-#548, an unclamped `Retry-After: u64::MAX`
        // could reach here) must not panic the u128->u64 cast or the add —
        // saturating arithmetic caps it at `u64::MAX` millis instead.
        let (_dir, path) = tmp_state_path();
        set_shared_retry_after(&path, Duration::from_secs(u64::MAX)).expect("set retry-after");
        match try_admit(&path, 100, 100, 111) {
            Ok(Admission::Wait(_)) => {}
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn cleared_retry_after_no_longer_blocks() {
        let (_dir, path) = tmp_state_path();
        set_shared_retry_after(&path, Duration::from_millis(1)).expect("set retry-after");
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            try_admit(&path, 100, 100, 111),
            Ok(Admission::Admitted(_))
        ));
    }

    #[test]
    fn a_killed_instances_lease_expires_and_recovers_its_slot() {
        // Simulate a crashed process: a lease already past its TTL, alone
        // saturating a concurrency: 1 cap. A fresh admission attempt from any
        // process must prune it and recover the slot rather than waiting
        // forever on a peer that no longer exists.
        let (_dir, path) = tmp_state_path();
        let stale = SharedState {
            request_times_ms: Vec::new(),
            retry_after_until_ms: None,
            leases: vec![Lease {
                id: 999,
                pid: 12345, // a pid that no longer runs, by construction of the test
                expires_at_ms: now_ms().saturating_sub(1_000),
            }],
        };
        fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();

        assert!(matches!(
            try_admit(&path, 100, 1, 111),
            Ok(Admission::Admitted(_))
        ));
    }

    #[test]
    fn a_live_leases_slot_is_not_reclaimed_early() {
        let (_dir, path) = tmp_state_path();
        let fresh = SharedState {
            request_times_ms: Vec::new(),
            retry_after_until_ms: None,
            leases: vec![Lease {
                id: 1,
                pid: 111,
                expires_at_ms: now_ms() + 60_000,
            }],
        };
        fs::write(&path, serde_json::to_vec(&fresh).unwrap()).unwrap();

        assert!(matches!(
            try_admit(&path, 100, 1, 222),
            Ok(Admission::Wait(_))
        ));
    }

    #[test]
    fn unwritable_state_dir_falls_back_to_in_process() {
        // A path whose "parent directory" is actually a plain file can never
        // be created as a directory — `with_locked_state` must report Err,
        // not panic, so the caller falls back to in-process-only gating.
        let dir = tempfile::tempdir().expect("tempdir");
        let blocking_file = dir.path().join("not_a_dir");
        fs::write(&blocking_file, b"x").unwrap();
        let path = blocking_file.join("sub").join("endpoint.state");

        assert!(matches!(try_admit(&path, 100, 100, 111), Err(())));
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

        let lease = gate
            .acquire(100, 1)
            .await
            .expect("shared state dir is writable in this test");
        // Cap is 1 and the lease above holds it — a second acquire attempt
        // from the "same instance" must not be immediately admitted.
        let second = tokio::time::timeout(Duration::from_millis(50), gate.acquire(100, 1)).await;
        assert!(
            second.is_err(),
            "expected the second acquire to still be waiting"
        );
        drop(lease);
        // Give the lease's async release task a moment to run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let third = tokio::time::timeout(Duration::from_secs(2), gate.acquire(100, 1)).await;
        assert!(third.is_ok(), "slot must be released once the lease drops");
    }
}
