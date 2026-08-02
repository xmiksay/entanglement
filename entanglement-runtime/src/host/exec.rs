//! Shared process-execution plumbing for the exec tools (`bash`/`call`).
//!
//! Both tools run a model-authored command under a timeout. Killing only the
//! direct child (what `kill_on_drop` does) leaves anything the command itself
//! spawned — a server, a shell pipeline, a `&`-backgrounded job — as an orphan
//! that survives the tool call (#168). The fix is a process group: the child is
//! spawned as its own group leader (`process_group(0)`, `setsid`-style) so the
//! whole tree shares one negative pgid, and on timeout/cancel we SIGKILL that
//! group in one shot.

use std::process::Output;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

/// Outcome of running a child to completion or aborting it on timeout.
pub enum ExecOutcome {
    /// Child exited (any status); its fully-drained stdout/stderr. `io_error`
    /// is set when a pipe read failed before EOF (a genuine I/O error, not the
    /// clean EOF a process exit produces) — the buffers then hold a silently
    /// truncated prefix that callers must flag distinctly from normal output
    /// (#586).
    Completed {
        output: Output,
        io_error: Option<String>,
    },
    /// The timeout elapsed and the process group was killed. Carries the
    /// stdout/stderr captured *before* the kill — the prefix a slow command
    /// printed is often the diagnostic the model needs, so it must not be
    /// discarded along with the process (#169). `io_error` as above (#586).
    TimedOut {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        io_error: Option<String>,
    },
}

/// Put the child in its own process group so its entire descendant tree can be
/// signalled at once (#168). No-op off Unix, where `kill_on_drop` on the direct
/// child is the only guarantee available.
pub fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
}

/// SIGKILL the child's process group if this guard is dropped while still armed.
/// A `Stop` (#167) aborts the executor task, which drops the future holding the
/// running child; `kill_on_drop` would then reap only the direct child, leaving
/// grandchildren orphaned exactly as on the timeout path (#168). Disarmed on
/// every normal exit of [`wait_or_kill_group`], so it only fires on a cancel.
struct GroupKillGuard {
    /// Group leader pid (== pgid, since the child was spawned `process_group(0)`),
    /// or `None` once disarmed / when the child had no pid.
    pgid: Option<u32>,
}

impl GroupKillGuard {
    fn new(pgid: Option<u32>) -> Self {
        Self { pgid }
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pgid {
            kill_process_group(pid);
        }
    }
}

/// Run `child` to completion or until `dur` elapses, draining stdout+stderr
/// concurrently into buffers. On timeout the child's whole process group is
/// SIGKILLed (#168) so grandchildren don't orphan, then the output captured so
/// far is returned as [`ExecOutcome::TimedOut`] (#169) — the reader tasks hit
/// EOF once the group dies and hand back the accumulated prefix. If the future
/// is instead dropped mid-wait (a `Stop`-driven task abort, #167), the
/// [`GroupKillGuard`] kills the same group so cancellation matches the timeout's
/// containment rather than orphaning grandchildren under `kill_on_drop`.
pub async fn wait_or_kill_group(mut child: Child, dur: Duration) -> std::io::Result<ExecOutcome> {
    // Take the pipes and drain them in background tasks so a timeout doesn't
    // discard whatever the command already printed — we join the readers to
    // recover the buffered prefix on both the completed and timed-out paths.
    let pid = child.id();
    // Armed until a normal exit disarms it: fires the group kill if this frame is
    // dropped mid-wait (the executor task was aborted by a `Stop`).
    let mut kill_guard = GroupKillGuard::new(pid);
    let out_task = tokio::spawn(drain(child.stdout.take()));
    let err_task = tokio::spawn(drain(child.stderr.take()));

    match tokio::time::timeout(dur, child.wait()).await {
        Ok(Ok(status)) => {
            kill_guard.disarm();
            let (stdout, out_err) = out_task.await.unwrap_or_default();
            let (stderr, err_err) = err_task.await.unwrap_or_default();
            Ok(ExecOutcome::Completed {
                output: Output {
                    status,
                    stdout,
                    stderr,
                },
                io_error: combine_io_errors(out_err, err_err),
            })
        }
        Ok(Err(e)) => {
            kill_guard.disarm();
            Err(e)
        }
        Err(_) => {
            // The timeout owns the kill explicitly (it must precede the drain to
            // close the pipes); disarm so the guard doesn't double-signal.
            kill_guard.disarm();
            #[cfg(unix)]
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            #[cfg(not(unix))]
            let _ = pid;
            // The group is dead, so every write end of the pipes is closed; the
            // readers return the prefix captured before the kill.
            let (stdout, out_err) = out_task.await.unwrap_or_default();
            let (stderr, err_err) = err_task.await.unwrap_or_default();
            Ok(ExecOutcome::TimedOut {
                stdout,
                stderr,
                io_error: combine_io_errors(out_err, err_err),
            })
        }
    }
}

/// Read a child pipe to EOF into a buffer, returning whatever was captured
/// alongside the read error if one cut the drain short (#586) — a caller must
/// be able to tell that apart from a clean EOF/byte-cap truncation instead of
/// silently treating a short read as the whole story.
async fn drain<R: AsyncRead + Unpin>(reader: Option<R>) -> (Vec<u8>, Option<std::io::Error>) {
    let mut buf = Vec::new();
    if let Some(mut r) = reader {
        if let Err(e) = r.read_to_end(&mut buf).await {
            return (buf, Some(e));
        }
    }
    (buf, None)
}

/// Merge the stdout/stderr drain errors into one message, or `None` if both
/// pipes read cleanly to EOF.
fn combine_io_errors(
    stdout_err: Option<std::io::Error>,
    stderr_err: Option<std::io::Error>,
) -> Option<String> {
    match (stdout_err, stderr_err) {
        (None, None) => None,
        (Some(e), None) => Some(format!("stdout read failed: {e}")),
        (None, Some(e)) => Some(format!("stderr read failed: {e}")),
        (Some(o), Some(e)) => Some(format!("stdout read failed: {o}; stderr read failed: {e}")),
    }
}

/// Prepend a warning line to a formatted tool result when a stream drain hit a
/// genuine I/O error (#586) — distinct from the byte-cap/timeout truncation
/// already visible in the body, so a partial capture caused by a broken pipe
/// isn't silently mistaken for the whole output. Shared by `bash` and `call`.
pub fn with_io_warning(body: String, io_error: Option<String>) -> String {
    match io_error {
        Some(e) => format!("[warning: output capture failed — {e}]\n{body}"),
        None => body,
    }
}

/// SIGKILL every process in the group led by `pid` (pgid == leader pid, since
/// the child was spawned with `process_group(0)`). Signalling `-pid` targets the
/// whole group in one call — the leader plus any grandchildren it spawned. Best
/// effort: a failure means the group was already gone.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    // SAFETY: `kill(2)` with a negative pid and SIGKILL is a plain syscall with
    // no memory effects; the worst case is ESRCH (group already reaped).
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// #168: on timeout the child's whole process group must die — a grandchild
    /// the command backgrounded must not survive to complete its work. The
    /// grandchild here waits, then touches a marker; the timeout fires first, so
    /// with the group kill the marker never appears. Without it (only
    /// `kill_on_drop` on `sh`), the grandchild orphans and writes the marker.
    #[tokio::test]
    async fn timeout_kills_backgrounded_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 1 && touch {}) & sleep 300", marker.display());

        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        own_process_group(&mut cmd);
        let child = cmd.spawn().unwrap();

        let outcome = wait_or_kill_group(child, Duration::from_millis(200))
            .await
            .unwrap();
        assert!(matches!(outcome, ExecOutcome::TimedOut { .. }));

        // Wait past the grandchild's own delay: if the group kill worked it was
        // SIGKILLed mid-sleep and never touched the marker.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "backgrounded grandchild survived the timeout and wrote {}",
            marker.display()
        );
    }

    /// #167: a `Stop`-driven abort drops the wait future mid-run. The
    /// [`GroupKillGuard`] must then SIGKILL the whole group, matching the
    /// timeout's containment — otherwise a backgrounded grandchild orphans under
    /// bare `kill_on_drop` (which reaps only the direct child). Here the future is
    /// dropped by an outer timeout well before its own (long) deadline.
    #[tokio::test]
    async fn dropped_future_kills_backgrounded_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 1 && touch {}) & sleep 300", marker.display());

        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        own_process_group(&mut cmd);
        let child = cmd.spawn().unwrap();

        // A 300s inner deadline that never fires; the outer 200ms timeout drops
        // the future instead — the same drop an aborted executor task triggers.
        let fut = wait_or_kill_group(child, Duration::from_secs(300));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), fut)
                .await
                .is_err(),
            "the inner wait should still be running when we drop it"
        );

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "backgrounded grandchild survived the cancelled exec and wrote {}",
            marker.display()
        );
    }

    /// #169: output printed before the timeout must survive the group kill. The
    /// command emits a line, then sleeps past the deadline; the captured prefix
    /// must contain that line even though the process was SIGKILLed.
    #[tokio::test]
    async fn timeout_preserves_partial_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo early-diagnostic; sleep 300"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        own_process_group(&mut cmd);
        let child = cmd.spawn().unwrap();

        match wait_or_kill_group(child, Duration::from_millis(300))
            .await
            .unwrap()
        {
            ExecOutcome::TimedOut { stdout, .. } => {
                assert!(
                    String::from_utf8_lossy(&stdout).contains("early-diagnostic"),
                    "buffered prefix lost on timeout: {:?}",
                    String::from_utf8_lossy(&stdout)
                );
            }
            ExecOutcome::Completed { .. } => panic!("slept-past-deadline command should time out"),
        }
    }

    #[tokio::test]
    async fn completed_child_returns_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hi"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        own_process_group(&mut cmd);
        let child = cmd.spawn().unwrap();
        match wait_or_kill_group(child, Duration::from_secs(5))
            .await
            .unwrap()
        {
            ExecOutcome::Completed { output, io_error } => {
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
                assert!(io_error.is_none(), "clean read must not report an io_error");
            }
            ExecOutcome::TimedOut { .. } => panic!("fast command should not time out"),
        }
    }

    /// #586: a genuine pipe-read error (not a clean EOF) must surface as
    /// `io_error` rather than being silently swallowed into a truncated buffer
    /// indistinguishable from a normal short read.
    #[tokio::test]
    async fn drain_reports_read_error_distinct_from_eof() {
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tokio::io::AsyncRead;

        struct FailingReader;
        impl AsyncRead for FailingReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Err(std::io::Error::other("simulated pipe failure")))
            }
        }

        let (buf, err) = drain(Some(FailingReader)).await;
        assert!(buf.is_empty());
        let err = err.expect("a failing reader must yield Some(error)");
        assert!(err.to_string().contains("simulated pipe failure"));
    }
}
