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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use entanglement_core::{Holly, SessionId};
use rhai::{Dynamic, Engine};
use tokio::sync::mpsc;

use crate::pending::PendingDecisions;
use crate::script_ops::ScriptRegistry;
use crate::seam;
use crate::tool_runner::EscapeRoot;
use crate::tools::ToolRegistry;

use super::{
    configure_engine, data, register_bindings, result_line, service_bindings, BindingCall,
    BindingPolicy,
};

/// Register, reply with the handle, then run the script to completion with
/// output streaming into the registry. Runs on the executor-spawned task the
/// blocking path would have used — the task simply outlives its `ToolResult`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_background(
    holly: Holly,
    tools: ToolRegistry,
    policy: BindingPolicy,
    escape_root: Option<EscapeRoot>,
    session: SessionId,
    request_id: String,
    pending: PendingDecisions,
    script: String,
    timeout: std::time::Duration,
    stop: Arc<AtomicBool>,
    scripts: ScriptRegistry,
) {
    let secs = timeout.as_secs();
    let (id, op) = scripts.register(
        script_label(&script),
        Some(session.clone()),
        timeout,
        stop.clone(),
    );
    seam::reply(
        &holly,
        session.clone(),
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

    let (tx, rx) = mpsc::unbounded_channel::<BindingCall>();
    let start = Instant::now();
    let bash_enabled = tools.contains("bash");
    let engine_stop = stop.clone();
    let timed_out = Arc::new(AtomicBool::new(false));
    let engine_timed_out = timed_out.clone();
    let print_op = op.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut engine = Engine::new_raw();
        configure_engine(
            &mut engine,
            timeout,
            start,
            move |text| {
                print_op.append_output(text);
                print_op.append_output("\n");
            },
            engine_stop,
            engine_timed_out,
        );
        register_bindings(&mut engine, tx, bash_enabled, start, timeout);
        data::register_data_functions(&mut engine);
        engine.eval::<Dynamic>(&script)
    });

    // Unlike the blocking path, a `Stop`-unwound run still records a terminal
    // state: nothing else ever resolves this handle, so a poll of a killed
    // script must see *something* final rather than `running` forever.
    let _stopped = service_bindings(
        rx,
        &tools,
        &policy,
        escape_root.as_ref(),
        &holly,
        &session,
        &request_id,
        &pending,
        true,
    )
    .await;

    let eval_result = handle.await;
    if timed_out.load(Ordering::Relaxed) {
        op.mark_timed_out();
    }
    let (line, is_error) = result_line(eval_result);
    op.finish(&line, is_error);
}

/// The listing label for a script (#607): its first non-empty line, truncated
/// — a whole script would bloat every pending-operations reply.
fn script_label(script: &str) -> String {
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
