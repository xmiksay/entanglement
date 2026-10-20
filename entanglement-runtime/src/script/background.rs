//! The detached `rhai` path (#637, ADR-0185): `background: true` registers an
//! `x-` handle in the shared [`ScriptRegistry`], replies immediately, and runs
//! the engine exactly as the blocking path does — same sandbox, same binding
//! bridge, same permission grading — with two differences: `print` output
//! streams into the registry entry instead of buffering for the reply, and
//! session-state transitions are suppressed (`detached`), since there is no
//! live turn for them to describe.
//!
//! A session `Stop` does not reach a background script (the executor skips the
//! canceller registration — [`super::is_background`]); the only kill is the
//! cooperative stop flag a `poll` `kill: true` trips, with the documented
//! ADR-0161 §5 limit that an in-flight `exec`/`bash` binding call finishes its
//! own budget-clamped timeout first. Mid-run binding `Ask`s stay enabled: the
//! `ToolRequest` → `Approve`/`Reject` round-trip works detached, parking the
//! script (whose deadline keeps counting) until the head answers.

use std::sync::atomic::Ordering;

use crate::seam;
use crate::tool_runner::Dispatcher;

use super::engine::{run_engine, RunSpec};
use super::{result_line, Launch};

/// Register, reply with the handle, then run the script to completion with
/// output streaming into the registry. Runs on the executor-spawned task the
/// blocking path would have used — the task simply outlives its `ToolResult`.
/// `label` names the run in the pending-operations listing; `run` is
/// already `detached`.
pub(super) async fn run_background(
    mut dispatcher: Dispatcher,
    launch: Launch,
    request_id: String,
    label: String,
    run: RunSpec,
) {
    let timeout = run.timeout;
    let secs = timeout.as_secs();
    let timed_out = run.timed_out.clone();
    let (id, op) = launch.scripts.register(
        label,
        Some(dispatcher.session.clone()),
        timeout,
        run.stop.clone(),
    );
    seam::reply(
        &dispatcher.holly,
        dispatcher.session.clone(),
        request_id.clone(),
        format!(
            "[background script {id} started]\n\
             Poll with `poll` (handle=\"{id}\") for incremental print output; \
             the final poll carries the script's `=> result` (or error) line. \
             Pass kill=true to request a cooperative stop — the script ends at \
             its next operation, after any in-flight exec/bash binding \
             finishes. Stopped automatically after {secs}s if still running."
        ),
        false,
    )
    .await;

    let print_op = op.clone();
    // Unlike the blocking path, a `Stop`-unwound run still records a terminal
    // state: nothing else ever resolves this handle, so a poll of a killed
    // script must see *something* final rather than `running` forever.
    let (eval_result, _stopped) =
        run_engine(&mut dispatcher, &launch, &request_id, run, move |text| {
            print_op.append_output(text);
            print_op.append_output("\n");
        })
        .await;
    if timed_out.load(Ordering::Relaxed) {
        op.mark_timed_out();
    }
    let (line, is_error) = result_line(eval_result);
    op.finish(&line, is_error);
}

/// The listing label for an inline script (#607): its first non-empty line,
/// truncated — a whole script would bloat every pending-operations reply.
pub(super) fn script_label(script: &str) -> String {
    let line = script
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut label: String = line.chars().take(80).collect();
    if label.len() < line.len() {
        label.push('…');
    }
    format!("rhai: {label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_the_first_nonempty_line_truncated() {
        assert_eq!(script_label("\n  let x = 1;\nx + 1"), "rhai: let x = 1;");
        let long = "a".repeat(120);
        let label = script_label(&long);
        assert!(label.starts_with("rhai: aaa"));
        assert!(label.ends_with('…'));
        assert!(label.chars().count() <= 87);
        assert_eq!(script_label(""), "rhai: ");
    }
}
