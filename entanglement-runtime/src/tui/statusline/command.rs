//! `tui.statusline.command` (#762 part B): a user script renders the left
//! half of the top row.
//!
//! One background task owns every run. The App publishes the *current* job
//! (command + timeout + [`Payload`]) into a `watch` channel whenever it
//! changes — or forces a re-send when an exec tool finishes, since a script
//! may read state the payload doesn't carry (a dirty tree). `watch` keeps
//! only the latest value, so a burst of changes during a run collapses into
//! one follow-up run. The next run starts no earlier than
//! [`MIN_INTERVAL`] after the last one *started* and half of it after that
//! one *finished*, so the command runs at most once a second, never twice
//! at once, and a script slower than the interval still leaves the machine
//! a gap instead of running back to back.
//!
//! A run is the hooks' execution model (`hooks::invoke`): `sh -c` in its own
//! process group, the payload as JSON on stdin, the whole group SIGKILLed on
//! timeout (then reaped — no zombie). Unlike a hook, only the first stdout
//! line matters, read through a [`READ_CAP`]-byte window while the rest of
//! the output is drained and discarded, so a chatty script can't grow memory
//! or block on a full pipe. ANSI escapes, control characters and bidi /
//! zero-width format characters are stripped: the theme's `status.text`
//! slot colours the line, and nothing can reorder or hide text in the row.
//! As with `bash` and hooks, the group is only killed on timeout or
//! cancel: a grandchild that redirects its output away (`cmd >/dev/null &`)
//! keeps running after a successful run.
//!
//! Environment: inherited like a hook's, minus the catalog's provider API
//! key variables (#164 — the same scrub `bash`/`call`/MCP stdio children
//! get); a status script has no business holding an LLM key.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::payload::Payload;
use crate::host::exec::{own_process_group, GroupKillGuard};
use crate::tui::event::Event;

/// At most one run starts per this interval.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Bytes of stdout (and stderr, for the failure reason) kept per run.
pub(crate) const READ_CAP: usize = 4096;

/// What the runner should be running right now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Job {
    pub command: String,
    pub timeout: Duration,
    pub payload: Payload,
}

/// The App's handle: `None` while no command is configured.
pub(crate) type JobTx = watch::Sender<Option<Job>>;

/// One finished run, posted back as [`Event::StatusCommand`]. `command`
/// lets the App drop a result for a command a reload already replaced.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    pub command: String,
    /// The cleaned first stdout line, or why the run failed.
    pub result: Result<String, String>,
}

/// App-held state: the runner handle, the last good line, and whether a
/// failure streak is on (the template shows; its one toast already fired).
#[derive(Default)]
pub(crate) struct CommandState {
    pub jobs: Option<JobTx>,
    pub line: Option<String>,
    pub failing: bool,
}

/// Start the runner. It stops when the TUI drops its event receiver (a run
/// in flight is killed with it) or the App drops the returned sender.
pub(crate) fn spawn_runner(event_tx: mpsc::Sender<Event>, secret_env: Vec<String>) -> JobTx {
    spawn_with(event_tx, secret_env, MIN_INTERVAL)
}

fn spawn_with(
    event_tx: mpsc::Sender<Event>,
    secret_env: Vec<String>,
    min_interval: Duration,
) -> JobTx {
    let (tx, mut rx) = watch::channel(None::<Job>);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = event_tx.closed() => return,
                changed = rx.changed() => if changed.is_err() { return },
            }
            let Some(job) = rx.borrow_and_update().clone() else {
                continue;
            };
            let started = Instant::now();
            let result = tokio::select! {
                // Dropping the run future fires its group-kill guard.
                _ = event_tx.closed() => return,
                r = run(&job, &secret_env) => r,
            };
            let outcome = Outcome {
                command: job.command,
                result,
            };
            if event_tx.send(Event::StatusCommand(outcome)).await.is_err() {
                return;
            }
            let next = (started + min_interval).max(Instant::now() + min_interval / 2);
            tokio::select! {
                _ = event_tx.closed() => return,
                _ = tokio::time::sleep_until(next) => {}
            }
        }
    });
    tx
}

async fn run(job: &Job, secret_env: &[String]) -> Result<String, String> {
    let mut body = serde_json::to_vec(&job.payload).map_err(|e| format!("payload: {e}"))?;
    // Newline-terminated, so a shell `read` (or `head -n 1`) sees a line.
    body.push(b'\n');
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&job.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for var in secret_env {
        cmd.env_remove(var);
    }
    entanglement_runtime::secret_env::scrub(&mut cmd);
    own_process_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("could not run: {e}"))?;
    let mut guard = GroupKillGuard::new(child.id());
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let feed = async move {
        if let Some(mut stdin) = stdin {
            // A script that never reads stdin closes the pipe early; that
            // broken pipe is normal, not a failure. Dropping closes stdin.
            let _ = stdin.write_all(&body).await;
        }
    };
    let work = async {
        let ((), out, err, status) =
            tokio::join!(feed, read_capped(stdout), read_capped(stderr), child.wait());
        (out, err, status)
    };
    match tokio::time::timeout(job.timeout, work).await {
        Ok((out, err, status)) => {
            let status = status.map_err(|e| format!("wait failed: {e}"))?;
            guard.disarm();
            if status.success() {
                return Ok(first_line(&out));
            }
            let what = match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "killed by a signal".to_string(),
            };
            match first_line(&err) {
                reason if reason.is_empty() => Err(what),
                reason => Err(format!("{what}: {reason}")),
            }
        }
        Err(_) => {
            drop(guard); // SIGKILL the whole group…
            let _ = child.wait().await; // …and reap the leader.
            Err(format!("timed out after {} ms", job.timeout.as_millis()))
        }
    }
}

/// Keep the first [`READ_CAP`] bytes, then drain the rest to EOF so the
/// child never blocks writing into a full pipe. A read error ends the read
/// like EOF: this is display text, a short prefix is still the best answer.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    let Some(mut reader) = reader else {
        return buf;
    };
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return buf,
            Ok(n) => {
                let room = READ_CAP.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
}

/// The first line of `bytes`, lossily decoded and sanitized.
pub(crate) fn first_line(bytes: &[u8]) -> String {
    let line = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
    sanitize(&String::from_utf8_lossy(line))
}

/// Drop ANSI escape sequences and control characters (a tab becomes a
/// space): raw escapes would corrupt the ratatui buffer, and colour comes
/// from the theme instead.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then one final byte in `@`..=`~`.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC/DCS/SOS/PM/APC: a string ended by BEL or `ESC \`.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // Any other escape is two characters long.
                _ => {}
            },
            '\t' => out.push(' '),
            c if c.is_control() || is_invisible_format(c) => {}
            c => out.push(c),
        }
    }
    out.trim_end().to_string()
}

/// Bidi embeddings/overrides/isolates and marks (which can visually
/// reorder the row) and zero-width characters (which can hide text).
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
#[path = "tests_command.rs"]
mod tests;
