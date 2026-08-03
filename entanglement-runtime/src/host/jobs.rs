//! Background-job registry shared by `bash` (the spawner) and `bash_output`
//! (the poller). A `bash` call with `run_in_background: true` spawns the command
//! in its own process group, registers it here, and returns immediately with a
//! job id; `bash_output` polls that id for the output captured **since the last
//! poll** plus the job's status (#170).
//!
//! Each job's stdout/stderr are drained incrementally by background tasks into
//! per-stream buffers. A poll drains those buffers (`mem::take`), so memory is
//! reclaimed on every read and only *unconsumed* output is retained — a long-
//! running dev server the model polls periodically stays bounded. Between polls
//! each buffer is still capped at [`MAX_JOB_BUFFER`]; overflow drops the
//! **oldest** bytes (the tail — the live tip — is what matters) and is counted so
//! the poll can report it.
//!
//! A *finished* job's entry — its command string and final exit state — used to
//! live in the map for the registry's entire lifetime (#621). It's now evicted
//! [`JOB_TTL`] after it finishes, or sooner if the number of finished entries
//! exceeds [`MAX_FINISHED_JOBS`] (oldest-finished evicted first). Eviction is a
//! lazy sweep run from `spawn`/`poll` rather than a dedicated background task —
//! the registry has no event loop of its own and both entry points are already
//! called at a cadence that keeps the map bounded. Running jobs are never
//! evicted; only a real process exit retires an entry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use entanglement_core::{DefaultIdGen, IdGen, IdKind};

/// Per-stream retention cap for a *not-yet-polled* background job. Bounds the
/// worst case where the model spawns a chatty job and never polls it. Generous
/// enough that a normal build/test run polled at a sane cadence never drops.
const MAX_JOB_BUFFER: usize = 256 * 1024;

/// How long a finished job's entry stays in the registry before eviction —
/// generous enough that a model polling at a normal cadence always sees the
/// final status before it's gone (#621).
const JOB_TTL: Duration = Duration::from_secs(15 * 60);

/// Hard cap on retained *finished* job entries, independent of `JOB_TTL` —
/// bounds a burst of many short jobs spawned faster than the TTL clears them.
const MAX_FINISHED_JOBS: usize = 200;

/// Terminal or in-progress state of a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    /// Process exited; `Some(code)` for a normal exit, `None` when killed by a
    /// signal (e.g. a `bash_output` `kill`).
    Exited(Option<i32>),
}

#[derive(Default)]
struct JobState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Bytes dropped from the front of each buffer since the last poll because
    /// the cap was hit — surfaced so a poll never silently loses output.
    stdout_dropped: u64,
    stderr_dropped: u64,
    finished: Option<Option<i32>>,
    /// Set by the timeout task (#617) when it kills the group because the job
    /// outran its bound — distinguishes an enforced deadline from a `kill=true`
    /// poll, which also lands as `Exited(None)`.
    timed_out: bool,
    /// Set alongside `finished`, once the process exits — the eviction clock
    /// starts here, not at spawn time (#621).
    finished_at: Option<Instant>,
}

struct Job {
    command: String,
    /// Group leader pid (== pgid). `None` if the child had no pid.
    pgid: Option<u32>,
    /// Wall-clock bound after which a still-running job is killed (#617) — the
    /// same `timeout` a foreground call takes, no longer ignored in the
    /// background path.
    timeout: Duration,
    state: Mutex<JobState>,
}

/// A single `bash_output` read: the output accumulated since the previous poll
/// plus the current status.
pub struct Poll {
    pub command: String,
    pub status: JobStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_dropped: u64,
    pub stderr_dropped: u64,
    /// The job was killed because it exceeded its timeout, not by an explicit
    /// `bash_output` `kill=true` poll (#617).
    pub timed_out: bool,
    pub timeout_secs: u64,
}

/// Shared, cheaply-cloned registry of background jobs. One instance is built at
/// startup and handed to both the `bash` spawner and the `bash_output` poller.
#[derive(Clone)]
pub struct JobRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    /// Mints `j-` job ids (ADR-0164) — replaces the old per-registry
    /// `AtomicU64` counter, which restarted at 0 on every fresh `JobRegistry`
    /// (the non-uniqueness bug ADR-0164 retires).
    id_gen: Arc<dyn IdGen>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            id_gen: Arc::new(DefaultIdGen::new()),
        }
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner::default()),
        }
    }
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `cmd` (already configured with cwd/stdio/process-group/env by the
    /// caller) as a background job, returning its id. Drain tasks capture its
    /// output incrementally; a reaper flips the status once the process exits
    /// **and** both streams have been fully drained, so a poll never reports
    /// `Exited` while output is still in flight. `timeout` bounds how long the
    /// job may run before its process group is killed (#617) — the same
    /// deadline a foreground `bash` call already enforces, previously ignored
    /// once `run_in_background` was set.
    pub fn spawn(
        &self,
        command: String,
        mut cmd: Command,
        timeout: Duration,
    ) -> std::io::Result<String> {
        self.evict_expired();
        let mut child = cmd.spawn()?;
        let pgid = child.id();
        let job = Arc::new(Job {
            command,
            pgid,
            timeout,
            state: Mutex::new(JobState::default()),
        });
        let id = self.inner.id_gen.next(IdKind::Job);
        self.inner
            .jobs
            .lock()
            .expect("job registry poisoned")
            .insert(id.clone(), job.clone());

        let out = tokio::spawn(drain(child.stdout.take(), job.clone(), Stream::Stdout));
        let err = tokio::spawn(drain(child.stderr.take(), job.clone(), Stream::Stderr));
        let deadline_job = job.clone();
        tokio::spawn(async move {
            // Wait for exit, then join the drains so every buffered byte lands
            // before the status flips — a poll seeing `Exited` has the full tail.
            let code = child.wait().await.ok().and_then(|s| s.code());
            let _ = out.await;
            let _ = err.await;
            let mut st = job.state.lock().expect("job state poisoned");
            st.finished = Some(code);
            st.finished_at = Some(Instant::now());
        });

        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let still_running = deadline_job
                .state
                .lock()
                .expect("job state poisoned")
                .finished
                .is_none();
            if still_running {
                #[cfg(unix)]
                if let Some(pid) = deadline_job.pgid {
                    super::exec::kill_process_group(pid);
                }
                deadline_job
                    .state
                    .lock()
                    .expect("job state poisoned")
                    .timed_out = true;
            }
        });
        Ok(id)
    }

    /// Remove finished job entries whose `JOB_TTL` has elapsed, then — if still
    /// over `MAX_FINISHED_JOBS` — evict the oldest-finished until back under it.
    /// Running jobs are never touched. Called from `spawn`/`poll` (the registry's
    /// only two entry points) so the map stays bounded without a dedicated sweep
    /// task (#621).
    fn evict_expired(&self) {
        let mut jobs = self.inner.jobs.lock().expect("job registry poisoned");
        sweep(&mut jobs, Instant::now(), JOB_TTL, MAX_FINISHED_JOBS);
    }

    /// Poll a job for output since the last poll. `kill` SIGKILLs the whole
    /// process group first. Returns `None` if the id is unknown.
    pub fn poll(&self, id: &str, kill: bool) -> Option<Poll> {
        self.evict_expired();
        let job = self
            .inner
            .jobs
            .lock()
            .expect("job registry poisoned")
            .get(id)
            .cloned()?;
        if kill {
            #[cfg(unix)]
            if let Some(pid) = job.pgid {
                super::exec::kill_process_group(pid);
            }
        }
        let mut guard = job.state.lock().expect("job state poisoned");
        let st = &mut *guard;
        let status = match st.finished {
            Some(code) => JobStatus::Exited(code),
            None => JobStatus::Running,
        };
        Some(Poll {
            command: job.command.clone(),
            status,
            stdout: std::mem::take(&mut st.stdout),
            stderr: std::mem::take(&mut st.stderr),
            stdout_dropped: std::mem::take(&mut st.stdout_dropped),
            stderr_dropped: std::mem::take(&mut st.stderr_dropped),
            timed_out: st.timed_out,
            timeout_secs: job.timeout.as_secs(),
        })
    }
}

/// Pure eviction logic, extracted so it's testable without waiting on a real
/// clock: remove finished entries older than `ttl`, then trim to `cap` by
/// dropping the oldest-finished first. A job with no `finished_at` (still
/// running) is never a candidate.
fn sweep(jobs: &mut HashMap<String, Arc<Job>>, now: Instant, ttl: Duration, cap: usize) {
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

enum Stream {
    Stdout,
    Stderr,
}

/// Read a child pipe in chunks, appending to the job's per-stream buffer as data
/// arrives (not `read_to_end`) so a poll mid-run sees the latest output.
async fn drain<R: AsyncRead + Unpin>(reader: Option<R>, job: Arc<Job>, which: Stream) {
    let Some(mut r) = reader else {
        return;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut guard = job.state.lock().expect("job state poisoned");
                let st = &mut *guard;
                match which {
                    Stream::Stdout => {
                        push_capped(&mut st.stdout, &mut st.stdout_dropped, &chunk[..n])
                    }
                    Stream::Stderr => {
                        push_capped(&mut st.stderr, &mut st.stderr_dropped, &chunk[..n])
                    }
                }
            }
        }
    }
}

/// Append `data`, dropping the oldest bytes past [`MAX_JOB_BUFFER`] so an unpolled
/// job can't grow without bound. Keeps the tail (the live tip) and counts what it
/// dropped.
fn push_capped(buf: &mut Vec<u8>, dropped: &mut u64, data: &[u8]) {
    buf.extend_from_slice(data);
    if buf.len() > MAX_JOB_BUFFER {
        let overflow = buf.len() - MAX_JOB_BUFFER;
        buf.drain(0..overflow);
        *dropped += overflow as u64;
    }
}

#[cfg(test)]
mod tests;
