//! Rendering for `poll` results — the status-line-then-body text a joined
//! job (`j-`) or background script (`x-`, #637/ADR-0185) hands back to the
//! model, byte-capped head+tail via [`crate::host::bounded_result`]. Split out
//! of `poll/mod.rs` for the 400-line file cap.

use crate::host::jobs::{JobStatus, Poll as JobPoll};
use crate::script_ops::ScriptPoll;

/// Render a job poll's status + drained output — the same shape `bash_output`
/// used, byte-capped head+tail via [`crate::host::bounded_result`].
pub(super) fn format_job_poll(id: &str, poll: JobPoll) -> String {
    let status = match poll.status {
        JobStatus::Running => "running".to_string(),
        JobStatus::Exited(Some(code)) => format!("exited {code}"),
        JobStatus::Exited(None) => "exited (killed)".to_string(),
    };
    let mut header = format!("[job {id}: {status}]\n");
    if poll.timed_out {
        header.push_str(&format!(
            "[killed: timed out after {}s]\n",
            poll.timeout_secs
        ));
    }
    let mut body = String::new();
    if poll.stdout_dropped > 0 {
        body.push_str(&format!(
            "[{} bytes of older stdout dropped]\n",
            poll.stdout_dropped
        ));
    }
    let stdout = String::from_utf8_lossy(&poll.stdout);
    if !stdout.is_empty() {
        body.push_str(&stdout);
    }
    if poll.stderr_dropped > 0 {
        body.push_str(&format!(
            "[{} bytes of older stderr dropped]\n",
            poll.stderr_dropped
        ));
    }
    let stderr = String::from_utf8_lossy(&poll.stderr);
    if !stderr.is_empty() {
        body.push_str("[stderr]\n");
        body.push_str(&stderr);
    }
    if stdout.is_empty() && stderr.is_empty() {
        body.push_str("(no new output)\n");
    }
    crate::host::bounded_result(&header, body)
}

/// Render a script poll's status + drained output (#637, ADR-0185) — the same
/// destructive-delta shape as a job poll, with script-specific terminal
/// causes. A `kill` poll returns before the engine has noticed the stop flag,
/// so it says what was *requested* rather than reporting a still-`running`
/// status as if nothing happened.
pub(super) fn format_script_poll(id: &str, poll: ScriptPoll, killed: bool) -> String {
    let status = if poll.running {
        "running"
    } else if poll.stopped {
        "stopped (killed)"
    } else if poll.is_error {
        "error"
    } else {
        "done"
    };
    let mut header = format!("[script {id}: {status}]\n");
    if poll.timed_out {
        header.push_str(&format!(
            "[stopped: exceeded the {}s time limit]\n",
            poll.timeout_secs
        ));
    }
    if killed && poll.running {
        header.push_str(
            "[cooperative stop requested — the script ends at its next \
             operation; an in-flight exec/bash binding finishes first. Poll \
             again for the terminal state.]\n",
        );
    }
    let mut body = String::new();
    if poll.dropped > 0 {
        body.push_str(&format!(
            "[{} bytes of older output dropped]\n",
            poll.dropped
        ));
    }
    let output = String::from_utf8_lossy(&poll.output);
    if output.is_empty() {
        body.push_str("(no new output)\n");
    } else {
        body.push_str(&output);
    }
    crate::host::bounded_result(&header, body)
}
