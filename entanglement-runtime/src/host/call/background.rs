//! `call`'s `background: true` path (#606, ADR-0161 §1): spawn detached into
//! the shared [`JobRegistry`] — the same mechanism `bash` uses — and return a
//! job id immediately instead of waiting on the child. Split out of `mod.rs`
//! (issue #451) — this owns only the detached-spawn path, independent of the
//! foreground wait/persist logic. `stdin` is unconditionally closed: the
//! caller refuses `background: true` together with `input_file`/`output_file`
//! before this runs, so there is never data to pipe in or an artifact to
//! write.

use crate::host::exec::own_process_group;
use crate::host::jobs::JobRegistry;
use crate::host::sandbox;
use crate::policy::SandboxResolver;
use anyhow::{Context, Result};
use entanglement_core::SessionId;
use std::path::Path;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_background(
    root: &Path,
    sandbox_resolver: &dyn SandboxResolver,
    secret_env: &[String],
    jobs: &JobRegistry,
    session: Option<&SessionId>,
    cwd: &Path,
    command: &str,
    args: &[String],
    dur: Duration,
) -> Result<String> {
    let policy = sandbox_resolver.resolve(session);
    let mut cmd = sandbox::command(&policy, root, command, args);
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    own_process_group(&mut cmd);
    for var in secret_env {
        cmd.env_remove(var);
    }
    let label = if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", args.join(" "))
    };
    let id = jobs
        .spawn(label, cmd, dur, session.cloned())
        .with_context(|| format!("spawning `{command}` in the background"))?;
    Ok(format!(
        "[background job {id} started]\n\
         Poll with `poll` (handle=\"{id}\") for incremental output; \
         pass kill=true to stop it. Killed automatically after {}s if \
         still running.",
        dur.as_secs()
    ))
}
