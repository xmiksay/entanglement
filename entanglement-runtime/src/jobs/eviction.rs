//! Pure job-registry eviction logic (#621), split out of `jobs.rs` to keep it
//! under the file-cap (issue #451) — a descendant module, so it still reads
//! `Job`'s private fields directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Job;

/// How long a finished job's entry stays in the registry before eviction —
/// generous enough that a model polling at a normal cadence always sees the
/// final status before it's gone (#621).
pub(super) const JOB_TTL: Duration = Duration::from_secs(15 * 60);

/// Hard cap on retained *finished* job entries, independent of `JOB_TTL` —
/// bounds a burst of many short jobs spawned faster than the TTL clears them.
pub(super) const MAX_FINISHED_JOBS: usize = 200;

/// Remove finished entries older than `ttl`, then trim to `cap` by dropping
/// the oldest-finished first. A job with no `finished_at` (still running) is
/// never a candidate. Extracted so it's testable without waiting on a real
/// clock.
pub(super) fn sweep(jobs: &mut HashMap<String, Arc<Job>>, now: Instant, ttl: Duration, cap: usize) {
    jobs.retain(|_, job| {
        job.state
            .lock()
            .expect("job state poisoned")
            .finished_at
            .map(|at| now.saturating_duration_since(at) < ttl)
            .unwrap_or(true)
    });
    if jobs.len() > cap {
        let mut finished: Vec<(String, Instant)> = jobs
            .iter()
            .filter_map(|(id, job)| {
                job.state
                    .lock()
                    .expect("job state poisoned")
                    .finished_at
                    .map(|at| (id.clone(), at))
            })
            .collect();
        finished.sort_by_key(|(_, at)| *at);
        for (id, _) in finished.into_iter().take(jobs.len() - cap) {
            jobs.remove(&id);
        }
    }
}
