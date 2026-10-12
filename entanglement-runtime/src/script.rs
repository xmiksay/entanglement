//! `rhai` — embedded, capability-sandboxed script engine (ADR-0046, amended by
//! ADR-0115 (exec bindings), ADR-0130 (`workdir`) and ADR-0227 (one binding
//! path, namespaces, `http()`, skill libraries)).
//!
//! A runtime-owned tool that runs a [Rhai](https://rhai.rs) script in one
//! call. A fresh engine has **no** filesystem, network, process or env
//! access: every capability is a function registered here, so the sandbox is
//! deny-by-default. The bindings are the host file tools, process exec
//! (`exec`/`bash`), `http()`, per-run generated `mcp::<server>::<tool>`
//! namespaces and each skill's `lib.rhai` as `skill::<skill>` — and every
//! binding call climbs the **same dispatch seam** as a model-issued call
//! ([`crate::tool_runner`]'s `Dispatcher`): mode grade, overlay, ceiling,
//! grants, escape-root, hooks, argument validation, lazy MCP connect,
//! `FileChange` capture. Nested `rhai`, the `mcp` envelope and every
//! runtime-owned tool are unreachable from a script.
//!
//! The engine is sync and the seam async, so the script runs under
//! [`tokio::task::spawn_blocking`] and each binding call crosses a bridge:
//! the blocking thread sends a [`BindingCall`] over an `mpsc` and blocks on a
//! `oneshot` reply that [`service`] fills from the seam's outcome — a refusal
//! is a catchable exception, a result a value. Core never sees a nested
//! call: only the outer `rhai` call is parked on a `ToolExec`.
//!
//! Resource bounds are by construction: `max_operations`, a wall-clock
//! timeout enforced by the progress callback (and clamped onto each nested
//! call), `max_call_levels`, string/array/map size caps. `import`/`eval` are
//! disabled.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{AgentState, Permission};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_names::RHAI_TOOL;
use crate::tool_runner::Dispatcher;

// The detached `background: true` path (#637, ADR-0185).
mod background;
// Pure JSON/YAML (de)serialization script functions — no IO, no permission
// check.
mod data;
// The shared engine run behind both the blocking and the background path.
mod engine;
// The model-facing tool spec and its binding reference.
mod spec;
// The sandboxed engine and its registered fixed bindings.
mod bindings;
// The per-run `mcp::…`/`skill::…` namespaces (ADR-0227).
mod namespaces;
// Print + return-value composition into the tool output.
mod output;
// The async side of the bridge: each binding call through the seam.
mod service;

pub use spec::rhai_spec;

use bindings::{configure_engine, register_bindings};
use output::{format_output, result_line};
use service::{await_approval, Approval};

/// Default wall-clock budget for a script, in seconds, when the model omits
/// `timeout`. Clamped to [`MAX_TIMEOUT_SECS`].
const DEFAULT_TIMEOUT_SECS: u64 = 5;
/// Upper bound on a caller-supplied `timeout` (seconds).
const MAX_TIMEOUT_SECS: u64 = 30;
/// Default/max budget for a `background: true` script (#637, ADR-0185) —
/// the background `bash`/`call` regime (#617).
const BG_DEFAULT_TIMEOUT_SECS: u64 = 120;
const BG_MAX_TIMEOUT_SECS: u64 = 600;

// Resource limits (see module docs).
const MAX_OPERATIONS: u64 = 10_000_000;
const MAX_CALL_LEVELS: usize = 64;
const MAX_STRING_SIZE: usize = 256 * 1024;
const MAX_ARRAY_SIZE: usize = 100_000;
const MAX_MAP_SIZE: usize = 100_000;

/// Parsed `rhai` tool input.
#[derive(Deserialize)]
struct ScriptInput {
    script: String,
    #[serde(default)]
    timeout: Option<u64>,
    /// Detach and return an `x-` handle instead of the result (#637).
    #[serde(default)]
    background: bool,
}

/// Whether a `rhai` call's input requests the detached path (#637) — read by
/// the executor to skip the session-Stop canceller registration: a
/// background script survives a session `Stop`, like a background job.
pub fn is_background(input: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| v.get("background").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// One binding call crossing the bridge: the registered tool to run, its
/// JSON input, and the channel the blocking thread parks on for the output
/// (`Ok`) or a refusal the script sees as a thrown exception (`Err`).
struct BindingCall {
    tool: String,
    input: String,
    reply: oneshot::Sender<Result<String, String>>,
}

/// What a run needs beyond the [`Dispatcher`], snapshotted by the executor
/// at launch.
pub(crate) struct Launch {
    /// The skill registry whose `lib.rhai` modules this run registers.
    pub skills: Arc<SkillRegistry>,
    /// `(project root, session workdir)` when the session works below the
    /// root (#764): a binding's relative path is rewritten onto it before it
    /// is graded, exactly as a direct call's is.
    pub workdir: Option<(PathBuf, PathBuf)>,
    pub scripts: crate::script_ops::ScriptRegistry,
}

/// Orchestrate one `rhai` call: gate the tool itself (`self_perm`), then run
/// the script with every binding going through `dispatcher`.
pub(crate) async fn run_rhai(
    dispatcher: Dispatcher,
    launch: Launch,
    self_perm: Permission,
    request_id: String,
    input: String,
    stop: Arc<AtomicBool>,
) {
    let holly = dispatcher.holly.clone();
    let session = dispatcher.session.clone();
    let parsed: ScriptInput = match serde_json::from_str(&input) {
        Ok(p) => p,
        Err(e) => {
            let out = format!("rhai: invalid input: {e}");
            seam::reply(&holly, session, request_id, out, true).await;
            return;
        }
    };
    let timeout = Duration::from_secs(if parsed.background {
        parsed
            .timeout
            .unwrap_or(BG_DEFAULT_TIMEOUT_SECS)
            .clamp(1, BG_MAX_TIMEOUT_SECS)
    } else {
        parsed
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS)
    });
    match self_perm {
        Permission::Deny => {
            let out = format!(
                "tool `{RHAI_TOOL}` denied by mode `{}` — use /mode to switch",
                dispatcher.mode
            );
            seam::reply(&holly, session, request_id, out, true).await;
            return;
        }
        Permission::Ask => {
            // The launch gate runs inside the live turn even for a background
            // script (the turn is parked on this very ToolExec).
            let approval = await_approval(
                &holly,
                &dispatcher.pending,
                &session,
                &request_id,
                RHAI_TOOL,
                &parsed.script,
            )
            .await;
            match approval {
                Approval::Approved => holly.emit_status(&session, AgentState::Thinking),
                Approval::Rejected(reason) => {
                    holly.emit_status(&session, AgentState::Thinking);
                    let out = format!("tool `{RHAI_TOOL}` rejected: {reason}");
                    seam::reply(&holly, session, request_id, out, true).await;
                    return;
                }
                // Stop unwinds silently: core cancels the turn on the same Stop.
                Approval::Stopped => return,
            }
        }
        Permission::Allow => {}
    }
    // The launch is the graded decision (ADR-0161 §3): a background script
    // detaches only after Allow/approval.
    if parsed.background {
        background::run_background(dispatcher, launch, request_id, parsed.script, timeout, stop)
            .await;
        return;
    }
    let prints = Arc::new(Mutex::new(String::new()));
    let sink = prints.clone();
    let run = engine::RunSpec {
        script: parsed.script,
        timeout,
        stop: stop.clone(),
        timed_out: Arc::new(AtomicBool::new(false)),
        detached: false,
    };
    let (eval_result, stopped) =
        engine::run_engine(&dispatcher, &launch, &request_id, run, move |text| {
            if let Ok(mut buf) = sink.lock() {
                buf.push_str(text);
                buf.push('\n');
            }
        })
        .await;
    // A `Stop` (#167) unwound the engine: the turn is being cancelled, so no
    // reply is owed.
    if stopped || stop.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let prints = prints.lock().map(|p| p.clone()).unwrap_or_default();
    let (output, is_error) = format_output(prints, eval_result);
    seam::reply(&holly, session, request_id, output, is_error).await;
}

#[cfg(test)]
mod tests;
