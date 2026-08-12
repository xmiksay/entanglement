//! Rendering for `poll` results — the status-line-then-body text a joined
//! job (`j-`) or background script (`x-`, #637/ADR-0185) hands back to the
//! model, byte-capped head+tail via [`crate::host::bounded_result`]. Split out
//! of `poll/mod.rs` for the 400-line file cap.

use super::retained::is_retained_handle;
use crate::host::jobs::{JobStatus, Poll as JobPoll};
use crate::script_ops::ScriptPoll;

/// The classified result of one `poll` call: the rendered text plus the two
/// structured side-channel fields `run_poll` folds into the `ToolResult`
/// (#695, closing the deferral ADR-0176/ADR-0186 left open). The #636 rule
/// applies: `is_error` marks a call that structurally never ran — an unknown
/// handle, a refused `kill`, a script that died in an error — while a bad
/// *outcome* of a poll that ran (a job exiting nonzero, a killed job or
/// script) is content, orthogonal to `exit_code` (ADR-0186).
pub(super) struct PollOutcome {
    pub(super) text: String,
    pub(super) exit_code: Option<i32>,
    pub(super) is_error: bool,
}

impl PollOutcome {
    /// A poll that ran and reports state — running/complete/list/paged.
    pub(super) fn ok(text: String) -> Self {
        Self {
            text,
            exit_code: None,
            is_error: false,
        }
    }

    /// A model mistake (ADR-0161 §2): unknown handle, refused `kill`, or a
    /// script's own terminal error.
    pub(super) fn err(text: String) -> Self {
        Self {
            text,
            exit_code: None,
            is_error: true,
        }
    }

    /// A job (`j-`) poll — the only kind that can observe a real exit status
    /// (#681, ADR-0186); `None` when still running or signal-killed.
    pub(super) fn exited(text: String, exit_code: Option<i32>) -> Self {
        Self {
            text,
            exit_code,
            is_error: false,
        }
    }
}

/// ADR-0161 §2: `kill: true` is refused on a sub-agent handle — cancelling a
/// child is a distinct authorization gate this ADR does not open. Also
/// refused on a retained-output handle (#608) — there is nothing running left
/// to kill.
pub(super) fn kill_refused_message(handle: &str) -> String {
    if is_retained_handle(handle) {
        format!(
            "poll: kill is not supported for retained-output handle `{handle}` \
             — the operation it pages already finished; there is nothing \
             running to kill."
        )
    } else {
        format!(
            "poll: kill is not supported for sub-agent handle `{handle}` — \
             cancelling a running sub-agent isn't available yet."
        )
    }
}

/// Unknown-handle error text (ADR-0161 §2): adopts `agent_poll`'s convention
/// of an error over `bash_output`'s "return it as text" — a poll for a handle
/// that doesn't exist (or isn't the caller's own) is a model mistake, not a
/// state report.
pub(super) fn unknown_handle(handle: &str) -> String {
    format!(
        "poll: unknown handle `{handle}` — it was never launched from this \
         session (use the id returned by bash/call/rhai/agent background=true, \
         or the retained-output id returned alongside a truncated call \
         result)."
    )
}

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

/// Classify + render a script poll (#637, ADR-0185; classification #695):
/// the `error` terminal state (uncaught exception or deadline — the same
/// classification the blocking `rhai` path reports, #636) is an error; a
/// cooperative stop ("stopped (killed)") is a state report, like a killed
/// job. Mirrors [`format_script_poll`]'s status choice.
pub(super) fn script_poll_outcome(id: &str, poll: ScriptPoll, killed: bool) -> PollOutcome {
    let is_error = !poll.running && !poll.stopped && poll.is_error;
    let text = format_script_poll(id, poll, killed);
    if is_error {
        PollOutcome::err(text)
    } else {
        PollOutcome::ok(text)
    }
}

/// Render a script poll's status + drained output (#637, ADR-0185) — the same
/// destructive-delta shape as a job poll, with script-specific terminal
/// causes. A `kill` poll returns before the engine has noticed the stop flag,
/// so it says what was *requested* rather than reporting a still-`running`
/// status as if nothing happened.
fn format_script_poll(id: &str, poll: ScriptPoll, killed: bool) -> String {
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
