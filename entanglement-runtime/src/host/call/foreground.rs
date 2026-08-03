//! `call`'s default (blocking) exec path: read an optional `input_file`,
//! resolve the output artifact, spawn, wait (or time out), persist the
//! output, and format the tailed result. Split out of `mod.rs` (issue #451)
//! — the counterpart to `background.rs`'s detached path.

use super::format::{format_call_output, format_call_streams};
use super::output::{persist_output, resolve_output_target};
use crate::host::exec::{own_process_group, wait_or_kill_group, with_io_warning, ExecOutcome};
use crate::host::resolve_under_root;
use crate::host::sandbox;
use crate::policy::SandboxResolver;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// `request_id` (#449) is forwarded to the escape-root grant check so a
/// `Once` approval for `workdir` is only consumed by the call it was approved
/// for.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_foreground(
    root: &Path,
    scratch_base: Option<&Path>,
    sandbox_resolver: &dyn SandboxResolver,
    secret_env: &[String],
    session: Option<&entanglement_core::SessionId>,
    cwd: &Path,
    command: &str,
    args: &[String],
    input_file: &Option<String>,
    output_file: &Option<String>,
    tail: u32,
    secs: u64,
    dur: Duration,
) -> Result<String> {
    // Validate + read input_file and resolve the output target *before*
    // spawning — a bad path (escape, missing input_file) must never launch
    // the child (#381, #386).
    let stdin_data = match input_file {
        Some(rel) => {
            let abs = resolve_under_root(root, rel)?;
            Some(
                tokio::fs::read(&abs)
                    .await
                    .with_context(|| format!("reading input_file `{rel}`"))?,
            )
        }
        None => None,
    };
    let output_target = resolve_output_target(root, scratch_base, output_file)?;

    let policy = sandbox_resolver.resolve(session);
    let mut cmd = sandbox::command(&policy, root, command, args);
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // No `input_file` → close stdin explicitly rather than inherit the
        // engine's real stdin, an unintentional leak until now (#381).
        .stdin(if stdin_data.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true);
    // Own process group so a timeout kills the whole tree, not just the
    // direct child (a launched server/pipeline would otherwise orphan — #168).
    own_process_group(&mut cmd);
    for var in secret_env {
        cmd.env_remove(var);
    }
    let mut child = cmd
        .spawn()
        // A missing binary (or non-exec target) surfaces here — return it as
        // tool output, never panic (ADR-0016 clean-error contract).
        .with_context(|| format!("spawning `{command}`"))?;

    // Feed stdin concurrently with draining stdout/stderr (below) so a
    // chatty child can't deadlock against a full pipe buffer either way.
    let stdin_task = match (child.stdin.take(), stdin_data) {
        (Some(mut stdin), Some(data)) => Some(tokio::spawn(async move {
            let _ = stdin.write_all(&data).await;
            // `stdin` drops here, closing the pipe (EOF) once fully written.
        })),
        _ => None,
    };

    let outcome = wait_or_kill_group(child, dur).await;
    if let Some(t) = stdin_task {
        let _ = t.await;
    }

    match outcome {
        Ok(ExecOutcome::Completed { output, io_error }) => {
            let notice = persist_output(&output_target, &output.stdout, &output.stderr).await?;
            Ok(with_io_warning(
                format_call_output(
                    output.status.code(),
                    &output.stdout,
                    &output.stderr,
                    tail,
                    &output_target.rel,
                    output_target.explicit,
                    notice,
                ),
                io_error,
            ))
        }
        // Return the output buffered before the kill (tailed like a normal
        // result) alongside the notice — the prefix is often the diagnostic
        // the model needs (#169). The artifacts get the same partial bytes.
        Ok(ExecOutcome::TimedOut {
            stdout,
            stderr,
            io_error,
        }) => {
            let notice = persist_output(&output_target, &stdout, &stderr).await?;
            Ok(with_io_warning(
                format_call_streams(
                    &format!("[killed: timed out after {secs}s]\n"),
                    &stdout,
                    &stderr,
                    tail,
                    &output_target.rel,
                    output_target.explicit,
                    notice,
                ),
                io_error,
            ))
        }
        Err(e) => Err(anyhow::anyhow!("call io error: {e}")),
    }
}
