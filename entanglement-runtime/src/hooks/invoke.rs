//! Running one hook command (#199): an `sh -c` child in its own process
//! group under a timeout, fed the JSON payload on stdin.

use std::process::Stdio;
use std::time::Duration;

use entanglement_core::SessionId;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{HookEvent, HookSpec};
use crate::host::exec::{own_process_group, wait_or_kill_group, ExecOutcome};

/// The result of running one hook command.
pub(super) struct Outcome {
    /// Exit code, or `None` if the process was killed / signalled.
    pub(super) code: Option<i32>,
    /// Whether the timeout fired and the group was killed.
    pub(super) timed_out: bool,
    /// Combined stdout+stderr, trimmed — surfaced as the block reason.
    pub(super) text: String,
    /// A genuine I/O error reading the hook's stdout/stderr (#586) — a pipe
    /// read failing outright, distinct from the clean EOF a process exit
    /// produces. Unlike a stdin *write* failure (see [`invoke`], which only
    /// logs one — a hook that never reads its input, e.g. `exit 0`, races the
    /// pipe close against the write and legitimately sees a broken pipe on
    /// every run), a read failure means the captured output itself is
    /// genuinely incomplete, so it always counts as a non-success.
    pub(super) io_error: Option<String>,
}

impl Outcome {
    /// A hook "succeeds" only on a clean exit 0 with no I/O error; a non-zero
    /// code, a signal, a timeout, or a genuine stdout/stderr read failure all
    /// count as a failure/veto (#586: a run with unreadable output must not
    /// silently pass).
    pub(super) fn succeeded(&self) -> bool {
        self.io_error.is_none() && !self.timed_out && self.code == Some(0)
    }

    /// A human-readable reason for a non-success, preferring an explicit I/O
    /// error (the hook's own output otherwise) over a bare status.
    pub(super) fn reason(&self) -> String {
        if let Some(e) = &self.io_error {
            return match self.text.is_empty() {
                true => e.clone(),
                false => format!("{e}; hook output: {}", self.text),
            };
        }
        if !self.text.is_empty() {
            return self.text.clone();
        }
        if self.timed_out {
            return "hook timed out".to_string();
        }
        match self.code {
            Some(c) => format!("hook exited with code {c}"),
            None => "hook killed by signal".to_string(),
        }
    }
}

/// A spawn failure (e.g. no `/bin/sh`) — reported as a non-success so a
/// `pre_tool_use` hook that can't even launch does not silently let the tool
/// through.
fn spawn_failure(err: impl std::fmt::Display) -> Outcome {
    Outcome {
        code: None,
        timed_out: false,
        text: format!("could not run hook: {err}"),
        io_error: None,
    }
}

/// Spawn one hook command, feed it `payload` on stdin, and collect its outcome.
/// Runs in its own process group under the spec's timeout (killing the whole
/// tree on expiry, like the exec tools, #168).
pub(super) async fn invoke(
    spec: &HookSpec,
    event: HookEvent,
    tool: Option<&str>,
    session: &SessionId,
    payload: serde_json::Value,
) -> Outcome {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&spec.command)
        .env("ENTANGLEMENT_HOOK_EVENT", event.as_str())
        .env("ENTANGLEMENT_SESSION_ID", &session.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(t) = tool {
        cmd.env("ENTANGLEMENT_TOOL_NAME", t);
    }
    crate::secret_env::scrub(&mut cmd);
    own_process_group(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return spawn_failure(e),
    };
    // Write the payload on a joined (not detached) task so a large input (a
    // whole-file `write`) can't deadlock against an unread stdout pipe — the
    // drain in `wait_or_kill_group` runs concurrently — while still letting us
    // observe a failed write (#586) instead of silently discarding it.
    // Dropping the handle closes stdin.
    let stdin_task = child.stdin.take().map(|mut stdin| {
        let body = payload.to_string();
        tokio::spawn(async move { stdin.write_all(body.as_bytes()).await })
    });

    let exec_outcome = wait_or_kill_group(child, Duration::from_secs(spec.timeout_secs)).await;
    let stdin_error = match stdin_task {
        Some(t) => t.await.ok().and_then(|r| r.err()),
        None => None,
    };
    if let Some(e) = stdin_error {
        // Not folded into `io_error`/`succeeded()`: a hook that never reads
        // its stdin (`exit 0`, `true`) races the pipe close against this
        // write and legitimately sees a broken pipe on *every* run — vetoing
        // on that would block tool calls for the common no-input hook
        // pattern. Logged so a hook that *does* depend on stdin and got a
        // truncated payload is still visible in the trace (#586).
        tracing::warn!(
            command = %spec.command,
            error = %e,
            "hook stdin write failed (payload may be truncated if the hook reads it)"
        );
    }

    match exec_outcome {
        Ok(ExecOutcome::Completed { output, io_error }) => Outcome {
            code: output.status.code(),
            timed_out: false,
            text: combine(&output.stdout, &output.stderr),
            io_error,
        },
        Ok(ExecOutcome::TimedOut {
            stdout,
            stderr,
            io_error,
        }) => Outcome {
            code: None,
            timed_out: true,
            text: combine(&stdout, &stderr),
            io_error,
        },
        Err(e) => spawn_failure(e),
    }
}

/// Merge a hook's stdout and stderr into one trimmed diagnostic string.
fn combine(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(stdout).trim().to_string();
    let err = String::from_utf8_lossy(stderr);
    let err = err.trim();
    if !err.is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(err);
    }
    s
}
