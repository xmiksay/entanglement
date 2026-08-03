//! Background-job registry shared by `bash`/`call` (the spawners) and `poll`
//! (the joiner, #605). A `bash`/`call` call with `background: true` spawns the
//! command in its own process group, registers it here owned by the spawning
//! session, and returns immediately with a job id; `poll` waits on that id for
//! the output captured **since the last poll** plus the job's status.
//!
//! Each job's stdout/stderr are drained incrementally by background tasks into
//! per-stream buffers. A poll drains those buffers (`mem::take`), so memory is
//! reclaimed on every read and only *unconsumed* output is retained. Between
//! polls each buffer is capped at [`MAX_JOB_BUFFER`]; overflow drops the
//! **oldest** bytes (the live tip is what matters) and is counted so the poll
//! can report it. A *finished* job's entry is evicted [`JOB_TTL`] after it
//! finishes, or sooner past [`MAX_FINISHED_JOBS`] (oldest-finished first) —
//! a lazy sweep from `spawn`/`poll`, not a dedicated task (#621). Running jobs
//! are never evicted; only a real process exit retires an entry.
//!
//! **Waiting (#605, ADR-0161 §2).** `poll` used to be a synchronous drain with
//! no wakeup mechanism — a poll issued right after spawn just saw "no new
//! output" and the model busy-waited across turns. Each [`Job`] now carries a
//! [`tokio::sync::Notify`], woken by the drain tasks on every chunk of output
//! and by the reaper on exit, so [`JobRegistry::poll`] can block up to its
//! `timeout_secs` for *new output or exit* — whichever comes first — instead of
//! returning instantly with nothing to report.
//!
//! **Ownership (#605, ADR-0161 §4).** A job is owned by the [`SessionId`] that
//! spawned it (`None` for the session-less [`crate::tools::Tool::run`] path —
//! visible to any caller then). `poll` treats a wrong-owner id like an
//! unknown one, mirroring [`crate::agent_registry::AgentRegistry`]'s scoping (#618).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Notify;

use entanglement_core::{DefaultIdGen, IdGen, IdKind, SessionId};

use eviction::{sweep, JOB_TTL, MAX_FINISHED_JOBS};

mod eviction;
mod list;

pub use list::JobOpInfo;

/// Per-stream retention cap for a *not-yet-polled* background job. Bounds the
/// worst case where the model spawns a chatty job and never polls it. Generous
/// enough that a normal build/test run polled at a sane cadence never drops.
const MAX_JOB_BUFFER: usize = 256 * 1024;

/// Terminal or in-progress state of a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    /// Process exited; `Some(code)` for a normal exit, `None` when killed by a
    /// signal (e.g. a `poll` `kill`).
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
    /// The session that spawned this job (#605) — `None` (session-less
    /// `Tool::run`) leaves the job visible to any poller.
    owner: Option<SessionId>,
    /// Group leader pid (== pgid). `None` if the child had no pid.
    pgid: Option<u32>,
    /// Wall-clock bound after which a still-running job is killed (#617) — the
    /// same `timeout` a foreground call takes, no longer ignored in background.
    timeout: Duration,
    /// Launch instant, for a #607 pending-operations listing's elapsed time.
    started: Instant,
    state: Mutex<JobState>,
    /// Woken on every new chunk of output and on exit (#605) — what lets `poll`
    /// wait instead of busy-draining an empty buffer.
    notify: Notify,
}

/// A single `poll` read: the output accumulated since the previous poll plus
/// the current status.
pub struct Poll {
    pub command: String,
    pub status: JobStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_dropped: u64,
    pub stderr_dropped: u64,
    /// The job was killed because it exceeded its timeout, not by an explicit
    /// `poll` `kill=true` (#617).
    pub timed_out: bool,
    pub timeout_secs: u64,
}

/// Shared, cheaply-cloned registry of background jobs. One instance is built at
/// startup and handed to both the `bash` spawner and the `poll` joiner.
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
    /// caller) as a background job owned by `owner`, returning its id. Drain
    /// tasks capture its output incrementally; a reaper flips the status once
    /// the process exits **and** both streams have been fully drained, so a
    /// poll never reports `Exited` while output is still in flight. `timeout`
    /// bounds how long the job may run before its process group is killed
    /// (#617) — the same deadline a foreground `bash`/`call` invocation already
    /// enforces, previously ignored once `background` was set.
    pub fn spawn(
        &self,
        command: String,
        mut cmd: Command,
        timeout: Duration,
        owner: Option<SessionId>,
    ) -> std::io::Result<String> {
        self.evict_expired();
        let mut child = cmd.spawn()?;
        let pgid = child.id();
        let job = Arc::new(Job {
            command,
            owner,
            pgid,
            timeout,
            started: Instant::now(),
            state: Mutex::new(JobState::default()),
            notify: Notify::new(),
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
            {
                let mut st = job.state.lock().expect("job state poisoned");
                st.finished = Some(code);
                st.finished_at = Some(Instant::now());
            }
            // Wake any poll parked waiting for this exit (#605).
            job.notify.notify_waiters();
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

    /// Remove finished job entries past `JOB_TTL`, then trim to `MAX_FINISHED_JOBS`
    /// (oldest-finished first). Running jobs are never touched. Called from
    /// `spawn`/`poll` so the map stays bounded without a dedicated task (#621).
    fn evict_expired(&self) {
        let mut jobs = self.inner.jobs.lock().expect("job registry poisoned");
        sweep(&mut jobs, Instant::now(), JOB_TTL, MAX_FINISHED_JOBS);
    }

    /// Poll a job for output since the last poll, waiting up to `timeout_secs`
    /// (`0` = unbounded) for new output or exit if nothing is ready yet (#605).
    /// `caller` must own the job (or it must be ownerless) or this returns
    /// `None` — indistinguishable from an unknown id, so a poll never confirms
    /// another session's job exists. `kill` SIGKILLs the process group and
    /// returns immediately with whatever is buffered, without waiting for the
    /// kill to be reaped — unchanged from the pre-#605 synchronous behavior.
    pub async fn poll(
        &self,
        id: &str,
        caller: &SessionId,
        kill: bool,
        timeout_secs: u64,
    ) -> Option<Poll> {
        self.evict_expired();
        let job = self
            .inner
            .jobs
            .lock()
            .expect("job registry poisoned")
            .get(id)
            .cloned()?;
        if !owner_allows(&job.owner, caller) {
            return None;
        }
        if kill {
            #[cfg(unix)]
            if let Some(pid) = job.pgid {
                super::exec::kill_process_group(pid);
            }
            return Some(snapshot(&job));
        }
        let deadline =
            (timeout_secs != 0).then(|| Instant::now() + Duration::from_secs(timeout_secs));
        loop {
            // Registered *before* the ready-check (the standard `Notify`
            // pattern) so a notification fired between the check and the
            // `.await` below is never missed.
            let notified = job.notify.notified();
            if has_new(&job) {
                break;
            }
            match deadline {
                None => notified.await,
                Some(dl) => {
                    let remaining = dl.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let _ = tokio::time::timeout(remaining, notified).await;
                }
            }
        }
        Some(snapshot(&job))
    }
}

/// Whether `caller` may read a job owned by `owner` — an ownerless job (the
/// session-less `Tool::run` path) is visible to anyone; an owned job only to
/// its owner (#605).
fn owner_allows(owner: &Option<SessionId>, caller: &SessionId) -> bool {
    owner.as_ref().is_none_or(|o| o == caller)
}

/// Whether `job` has something a poll hasn't seen yet: unconsumed output or a
/// terminal status. Peeks without draining.
fn has_new(job: &Job) -> bool {
    let guard = job.state.lock().expect("job state poisoned");
    !guard.stdout.is_empty() || !guard.stderr.is_empty() || guard.finished.is_some()
}

/// Drain `job`'s buffers into a [`Poll`] snapshot (`mem::take`, so a poll is
/// destructive and incremental).
fn snapshot(job: &Job) -> Poll {
    let mut guard = job.state.lock().expect("job state poisoned");
    let st = &mut *guard;
    let status = match st.finished {
        Some(code) => JobStatus::Exited(code),
        None => JobStatus::Running,
    };
    Poll {
        command: job.command.clone(),
        status,
        stdout: std::mem::take(&mut st.stdout),
        stderr: std::mem::take(&mut st.stderr),
        stdout_dropped: std::mem::take(&mut st.stdout_dropped),
        stderr_dropped: std::mem::take(&mut st.stderr_dropped),
        timed_out: st.timed_out,
        timeout_secs: job.timeout.as_secs(),
    }
}

enum Stream {
    Stdout,
    Stderr,
}

/// Read a child pipe in chunks, appending to the job's per-stream buffer as data
/// arrives (not `read_to_end`) so a poll mid-run sees the latest output. Wakes
/// any parked poll (#605) after each chunk lands.
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
                job.notify.notify_waiters();
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
