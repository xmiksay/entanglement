//! The on-disk storage engine backing [`super::shared_state::SharedGate`]
//! (#523, ADR-0144) — split out from `shared_state` purely to keep both
//! files under the 400-line cap (#552). `SharedGate`/`SharedLease` own the
//! public API and policy (when to admit, what a caller does with a lease);
//! this module owns the file format and the locked read-modify-write
//! mechanics underneath it, mirroring
//! `entanglement-runtime::config::lock::with_locked_file` (independently
//! re-implemented since `entanglement-provider` is the leaf crate and takes
//! no `entanglement-*` dependency, ADR-0053).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long an unrenewed concurrency lease is honored before it's treated as
/// abandoned and its slot recovered. Kept close to the renewal interval
/// (~2×, #547) rather than generously above it — this bounds how long a
/// `SIGKILL`ed process's slot blocks the next launch, the one case a
/// lease's synchronous release-on-drop can't cover.
pub(super) const LEASE_TTL: Duration = Duration::from_secs(120);

/// Polling cadence while waiting on the shared RPM budget or concurrency cap
/// to free up. This is local sleep-then-retry, not a blocking wait — a
/// deliberately short interval costs one cheap locked file read/write per
/// tick, not a busy loop, and keeps a freed slot from sitting unnoticed long
/// after a shared `Retry-After` deadline or RPM window passes.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// What a single locked read-modify-write pass decided.
#[derive(Debug)]
pub(super) enum Admission {
    /// Admitted; holds the lease id to renew/release.
    Admitted(u64),
    /// Not admitted yet; retry after (at least) this long.
    Wait(Duration),
}

/// The file's on-disk shape. Deliberately minimal — a RPM ledger, live
/// leases, and a shared cool-down deadline are the whole cross-process
/// contract (ADR-0144).
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub(super) struct SharedState {
    #[serde(default)]
    pub(super) request_times_ms: Vec<u64>,
    #[serde(default)]
    pub(super) retry_after_until_ms: Option<u64>,
    #[serde(default)]
    pub(super) leases: Vec<Lease>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct Lease {
    pub(super) id: u64,
    pub(super) pid: u32,
    pub(super) expires_at_ms: u64,
}

/// A single locked read-modify-write attempt: prune stale entries, then
/// admit if the shared cool-down has cleared and both the RPM ledger and the
/// lease count have room. Combines all three checks into one lock
/// acquisition so a caller need not round-trip the file three times per
/// attempt.
pub(super) fn try_admit(
    path: &Path,
    rpm: u32,
    concurrency: usize,
    pid: u32,
) -> Result<Admission, ()> {
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

        // Stamps the RPM ledger at admission, which the caller (`execute_with_retry`,
        // #546) treats as "at send": it acquires this lease only after both
        // in-process permits, immediately before firing the request — so
        // nothing can queue between this timestamp and the actual send the
        // way a caller stuck on its model semaphore used to.
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

pub(super) fn renew_lease(path: &Path, lease_id: u64) -> Result<(), ()> {
    with_locked_state(path, |state| {
        let now = now_ms();
        for lease in state.leases.iter_mut() {
            if lease.id == lease_id {
                lease.expires_at_ms = now + LEASE_TTL.as_millis() as u64;
            }
        }
    })
}

pub(super) fn remove_lease(path: &Path, lease_id: u64) -> Result<(), ()> {
    with_locked_state(path, |state| {
        state.leases.retain(|lease| lease.id != lease_id);
    })
}

pub(super) fn set_shared_retry_after(path: &Path, delay: Duration) -> Result<(), ()> {
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

/// Delete `.state`/`.lock` pairs under `dir` that are both **idle** (the
/// `.state` file's mtime is older than `max_idle`) and **empty** (no live
/// lease, no pending cool-down, no request in the trailing RPM window, after
/// running the same [`prune`] every real admission attempt applies) — an
/// orphan left behind by a `/key` rotation or a catalog `base_url` change
/// (#551): nothing else ever removes these files, so without this sweep they
/// accumulate under the state directory forever. An endpoint still in active
/// use always fails the "empty" check (a live lease or a fresh RPM-window
/// timestamp) and is left untouched regardless of its mtime. Best-effort: a
/// read/write failure on any one pair is skipped rather than propagated, so
/// one bad file never stops the sweep. Returns the number of pairs removed.
pub(super) fn prune_orphaned(dir: &Path, max_idle: Duration) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = now_ms();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("state") {
            continue;
        }
        let is_idle = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    >= max_idle
            })
            .unwrap_or(false);
        if !is_idle {
            continue;
        }
        let is_empty = with_locked_state(&path, |state| {
            prune(state, now);
            state.leases.is_empty()
                && state.request_times_ms.is_empty()
                && state.retry_after_until_ms.is_none()
        });
        if is_empty != Ok(true) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            let _ = fs::remove_file(path.with_extension("lock"));
            removed += 1;
        }
    }
    removed
}

/// Run `f` over the current on-disk state under an exclusive advisory lock on
/// `path`'s `.lock` sibling, then persist whatever `f` mutated. Creates the
/// parent directory on first use. `Err(())` covers every filesystem failure
/// (unwritable directory, permissions, a read-only filesystem) uniformly —
/// the caller's only reaction is "fall back to in-process gating," so the
/// specific cause isn't threaded through.
///
/// The temp file is `fsync`ed before the rename and the parent directory after
/// it, mirroring `entanglement-runtime::config::atomic::atomic_write` (#549) —
/// without both syncs a crash between write and rename can leave `path`
/// zero-length rather than either the old or new content.
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
    {
        let mut tmp_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|_| ())?;
        tmp_file.write_all(&bytes).map_err(|_| ())?;
        tmp_file.sync_all().map_err(|_| ())?;
    }
    fs::rename(&tmp_path, path).map_err(|_| ())?;
    sync_dir(dir).map_err(|_| ())?;
    Ok(result)
}

/// Flush a directory's own metadata (the rename that just landed in it) to
/// disk. Windows has no directory handle to fsync — NTFS journals the rename
/// itself, so this is a no-op there.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

pub(super) fn now_ms() -> u64 {
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
pub(super) fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state_path() -> (tempfile::TempDir, std::path::PathBuf) {
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

    fn set_mtime_ago(path: &Path, ago: Duration) {
        let modified = SystemTime::now() - ago;
        fs::File::open(path)
            .expect("open for mtime backdate")
            .set_modified(modified)
            .expect("set mtime");
    }

    #[test]
    fn prune_orphaned_removes_idle_empty_pairs() {
        // #551: an old `/key` rotation's endpoint has gone completely quiet —
        // no live lease, no pending cool-down, no recent request — and its
        // `.state`/`.lock` pair has sat untouched past `max_idle`. It must be
        // swept, or it (and every other rotation before it) accumulates
        // forever.
        let (dir, path) = tmp_state_path();
        fs::write(&path, serde_json::to_vec(&SharedState::default()).unwrap()).unwrap();
        let lock_path = path.with_extension("lock");
        fs::write(&lock_path, b"").unwrap();
        set_mtime_ago(&path, Duration::from_secs(7200));

        let removed = prune_orphaned(dir.path(), Duration::from_secs(3600));
        assert_eq!(removed, 1);
        assert!(!path.exists());
        assert!(!lock_path.exists());
    }

    #[test]
    fn prune_orphaned_leaves_a_recently_touched_pair_alone() {
        // An endpoint that's merely idle for a few seconds (well under
        // `max_idle`) is not yet "orphaned" — deleting it would just make the
        // very next request rebuild it from scratch, discarding real state.
        let (dir, path) = tmp_state_path();
        fs::write(&path, serde_json::to_vec(&SharedState::default()).unwrap()).unwrap();

        let removed = prune_orphaned(dir.path(), Duration::from_secs(3600));
        assert_eq!(removed, 0);
        assert!(path.exists());
    }

    #[test]
    fn prune_orphaned_leaves_an_idle_but_still_live_pair_alone() {
        // Idle by mtime, but still holding a live (unexpired) lease — a
        // long-running streamed request whose heartbeat just hasn't ticked
        // yet must never be swept out from under it.
        let (dir, path) = tmp_state_path();
        let state = SharedState {
            request_times_ms: Vec::new(),
            retry_after_until_ms: None,
            leases: vec![Lease {
                id: 1,
                pid: 111,
                expires_at_ms: now_ms() + 60_000,
            }],
        };
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        set_mtime_ago(&path, Duration::from_secs(7200));

        let removed = prune_orphaned(dir.path(), Duration::from_secs(3600));
        assert_eq!(removed, 0);
        assert!(path.exists());
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
}
