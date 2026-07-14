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

use tokio::process::{Child, Command};

/// Outcome of running a child to completion or aborting it on timeout.
pub enum ExecOutcome {
    /// Child exited (any status); its fully-drained stdout/stderr.
    Completed(Output),
    /// The timeout elapsed and the process group was killed.
    TimedOut,
}

/// Put the child in its own process group so its entire descendant tree can be
/// signalled at once (#168). No-op off Unix, where `kill_on_drop` on the direct
/// child is the only guarantee available.
pub fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
}

/// Run `child` to completion or until `dur` elapses, draining stdout+stderr
/// concurrently. On timeout the child's whole process group is SIGKILLed (#168)
/// so grandchildren don't orphan, then [`ExecOutcome::TimedOut`] is returned.
/// `kill_on_drop` (set by the caller) still covers the plain-cancellation drop
/// path.
pub async fn wait_or_kill_group(child: Child, dur: Duration) -> std::io::Result<ExecOutcome> {
    // Capture the pid before `wait_with_output` consumes the child: on timeout
    // the future is dropped (killing the leader via `kill_on_drop`), but the
    // group's other members need the pgid to be reaped.
    let pid = child.id();
    match tokio::time::timeout(dur, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(ExecOutcome::Completed(output)),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            #[cfg(not(unix))]
            let _ = pid;
            Ok(ExecOutcome::TimedOut)
        }
    }
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
        assert!(matches!(outcome, ExecOutcome::TimedOut));

        // Wait past the grandchild's own delay: if the group kill worked it was
        // SIGKILLed mid-sleep and never touched the marker.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "backgrounded grandchild survived the timeout and wrote {}",
            marker.display()
        );
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
            ExecOutcome::TimedOut => panic!("fast command should not time out"),
        }
    }
}
