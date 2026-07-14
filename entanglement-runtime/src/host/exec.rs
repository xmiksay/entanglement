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
    /// Child exited (any status); its fully-drained stdout/stderr.
    Completed(Output),
    /// The timeout elapsed and the process group was killed. Carries the
    /// stdout/stderr captured *before* the kill — the prefix a slow command
    /// printed is often the diagnostic the model needs, so it must not be
    /// discarded along with the process (#169).
    TimedOut { stdout: Vec<u8>, stderr: Vec<u8> },
}

/// Put the child in its own process group so its entire descendant tree can be
/// signalled at once (#168). No-op off Unix, where `kill_on_drop` on the direct
/// child is the only guarantee available.
pub fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
}

/// Run `child` to completion or until `dur` elapses, draining stdout+stderr
/// concurrently into buffers. On timeout the child's whole process group is
/// SIGKILLed (#168) so grandchildren don't orphan, then the output captured so
/// far is returned as [`ExecOutcome::TimedOut`] (#169) — the reader tasks hit
/// EOF once the group dies and hand back the accumulated prefix. `kill_on_drop`
/// (set by the caller) still covers the plain-cancellation drop path.
pub async fn wait_or_kill_group(mut child: Child, dur: Duration) -> std::io::Result<ExecOutcome> {
    // Take the pipes and drain them in background tasks so a timeout doesn't
    // discard whatever the command already printed — we join the readers to
    // recover the buffered prefix on both the completed and timed-out paths.
    let pid = child.id();
    let out_task = tokio::spawn(drain(child.stdout.take()));
    let err_task = tokio::spawn(drain(child.stderr.take()));

    match tokio::time::timeout(dur, child.wait()).await {
        Ok(Ok(status)) => {
            let stdout = out_task.await.unwrap_or_default();
            let stderr = err_task.await.unwrap_or_default();
            Ok(ExecOutcome::Completed(Output {
                status,
                stdout,
                stderr,
            }))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            #[cfg(not(unix))]
            let _ = pid;
            // The group is dead, so every write end of the pipes is closed; the
            // readers return the prefix captured before the kill.
            let stdout = out_task.await.unwrap_or_default();
            let stderr = err_task.await.unwrap_or_default();
            Ok(ExecOutcome::TimedOut { stdout, stderr })
        }
    }
}

/// Read a child pipe to EOF into a buffer, returning whatever was captured. A
/// read error yields the bytes accumulated so far rather than losing them.
async fn drain<R: AsyncRead + Unpin>(reader: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut r) = reader {
        let _ = r.read_to_end(&mut buf).await;
    }
    buf
}

/// SIGKILL every process in the group led by `pid` (pgid == leader pid, since
/// the child was spawned with `process_group(0)`). Signalling `-pid` targets the
/// whole group in one call — the leader plus any grandchildren it spawned. Best
/// effort: a failure means the group was already gone.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
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
            ExecOutcome::Completed(_) => panic!("slept-past-deadline command should time out"),
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
            ExecOutcome::Completed(output) => {
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
            }
            ExecOutcome::TimedOut { .. } => panic!("fast command should not time out"),
        }
    }
}
