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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Per-stream retention cap for a *not-yet-polled* background job. Bounds the
/// worst case where the model spawns a chatty job and never polls it. Generous
/// enough that a normal build/test run polled at a sane cadence never drops.
const MAX_JOB_BUFFER: usize = 256 * 1024;

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
}

struct Job {
    command: String,
    /// Group leader pid (== pgid). `None` if the child had no pid.
    pgid: Option<u32>,
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
}

/// Shared, cheaply-cloned registry of background jobs. One instance is built at
/// startup and handed to both the `bash` spawner and the `bash_output` poller.
#[derive(Clone, Default)]
pub struct JobRegistry {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    counter: AtomicU64,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `cmd` (already configured with cwd/stdio/process-group/env by the
    /// caller) as a background job, returning its id. Drain tasks capture its
    /// output incrementally; a reaper flips the status once the process exits
    /// **and** both streams have been fully drained, so a poll never reports
    /// `Exited` while output is still in flight.
    pub fn spawn(&self, command: String, mut cmd: Command) -> std::io::Result<String> {
        let mut child = cmd.spawn()?;
        let pgid = child.id();
        let job = Arc::new(Job {
            command,
            pgid,
            state: Mutex::new(JobState::default()),
        });
        let id = format!("bg-{}", self.inner.counter.fetch_add(1, Ordering::SeqCst));
        self.inner
            .jobs
            .lock()
            .expect("job registry poisoned")
            .insert(id.clone(), job.clone());

        let out = tokio::spawn(drain(child.stdout.take(), job.clone(), Stream::Stdout));
        let err = tokio::spawn(drain(child.stderr.take(), job.clone(), Stream::Stderr));
        tokio::spawn(async move {
            // Wait for exit, then join the drains so every buffered byte lands
            // before the status flips — a poll seeing `Exited` has the full tail.
            let code = child.wait().await.ok().and_then(|s| s.code());
            let _ = out.await;
            let _ = err.await;
            job.state.lock().expect("job state poisoned").finished = Some(code);
        });
        Ok(id)
    }

    /// Poll a job for output since the last poll. `kill` SIGKILLs the whole
    /// process group first. Returns `None` if the id is unknown.
    pub fn poll(&self, id: &str, kill: bool) -> Option<Poll> {
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
        })
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
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        super::super::exec::own_process_group(&mut cmd);
        cmd
    }

    #[tokio::test]
    async fn spawn_poll_captures_output_and_exit() {
        let reg = JobRegistry::new();
        let id = reg
            .spawn("echo hi".into(), sh("echo hi; echo boom 1>&2"))
            .unwrap();
        // Give the reaper time to finish and flip status.
        for _ in 0..50 {
            let p = reg.poll(&id, false).unwrap();
            if p.status == JobStatus::Exited(Some(0)) {
                assert_eq!(String::from_utf8_lossy(&p.stdout).trim(), "hi");
                assert_eq!(String::from_utf8_lossy(&p.stderr).trim(), "boom");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("job never reached Exited(0)");
    }

    #[tokio::test]
    async fn poll_is_incremental_then_drains() {
        let reg = JobRegistry::new();
        let id = reg
            .spawn("echo one; sleep 30".into(), sh("echo one; sleep 30"))
            .unwrap();
        // First poll eventually sees "one" while still running.
        let mut seen = false;
        for _ in 0..50 {
            let p = reg.poll(&id, false).unwrap();
            if String::from_utf8_lossy(&p.stdout).contains("one") {
                assert_eq!(p.status, JobStatus::Running);
                seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(seen, "first poll never saw the emitted line");
        // A poll drains the buffer, so the immediate next poll has no new output.
        let p2 = reg.poll(&id, false).unwrap();
        assert!(p2.stdout.is_empty(), "second poll should be drained");
        // Kill it so the test process group doesn't leak.
        let _ = reg.poll(&id, true);
    }

    #[tokio::test]
    async fn poll_unknown_job_is_none() {
        let reg = JobRegistry::new();
        assert!(reg.poll("bg-999", false).is_none());
    }

    #[test]
    fn push_capped_drops_oldest_over_cap() {
        let mut buf = Vec::new();
        let mut dropped = 0;
        let big = vec![b'x'; MAX_JOB_BUFFER + 100];
        push_capped(&mut buf, &mut dropped, &big);
        assert_eq!(buf.len(), MAX_JOB_BUFFER);
        assert_eq!(dropped, 100);
    }
}
