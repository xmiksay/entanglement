//! `rhai` — embedded, capability-sandboxed script engine (ADR-0046).
//!
//! A runtime-owned host tool that runs a [Rhai](https://rhai.rs) script in one
//! tool call — the sanctioned replacement for "shell out to `python3`/`node`
//! with a heredoc". A fresh Rhai engine has **no** filesystem, network, process
//! spawn, or env access: every capability is a Rust function we register, so the
//! sandbox is deny-by-default. The only capabilities bound are the root-contained
//! host quintet (`read`/`glob`/`grep`/`edit`/`write`), each routed through the
//! **same permission resolution as a model-issued tool call** (#59).
//!
//! Because the engine is sync and the permission round-trip is async, the script
//! runs under [`tokio::task::spawn_blocking`] and each binding call crosses a
//! small bridge: the blocking thread sends a [`BindingCall`] over an `mpsc` and
//! blocks on a `oneshot` reply from this module's async resolver, which resolves
//! `Allow | Ask | Deny`, runs the `ToolRequest` → `Approve`/`Reject` round-trip on
//! `Ask`, and executes the delegated [`ToolRegistry`] tool on `Allow`. Core is
//! untouched — like `ask_user`/`propose_plan` this is all inside the runtime
//! executor ([`crate::tool_runner`]).
//!
//! Resource bounds are by construction: `max_operations`, a wall-clock timeout
//! enforced by the progress callback, `max_call_levels`, and string/array/map
//! size caps — a runaway script terminates deterministically with a clear error,
//! never an OOM. `import`/`eval` are disabled so a script cannot pull in modules
//! or re-enter the parser.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use entanglement_core::{
    AgentProfile, AgentState, Holly, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
    ToolSpec,
};

use crate::tools::ToolRegistry;
use rhai::packages::{Package, StandardPackage};
use rhai::{Dynamic, Engine, EvalAltResult, Position};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;

use crate::host::truncate_output;
use crate::pending::{self, PendingDecisions};
use crate::permission::{min_permission, permission_arg, permission_chain, tool_masked};
use crate::seam;
use crate::subagent::SpawnGuard;
use crate::tool_names::{BINDING_TOOLS, RHAI_TOOL};

/// Default wall-clock budget for a script, in seconds, when the model omits
/// `timeout`. Clamped to [`MAX_TIMEOUT_SECS`].
const DEFAULT_TIMEOUT_SECS: u64 = 5;
/// Upper bound on a caller-supplied `timeout` (seconds).
const MAX_TIMEOUT_SECS: u64 = 30;

// Resource limits (see module docs). Generous enough for real multi-step logic,
// tight enough that a runaway script dies deterministically.
const MAX_OPERATIONS: u64 = 10_000_000;
const MAX_CALL_LEVELS: usize = 64;
const MAX_STRING_SIZE: usize = 256 * 1024;
const MAX_ARRAY_SIZE: usize = 100_000;
const MAX_MAP_SIZE: usize = 100_000;

/// The `rhai` tool schema advertised to the model. Appended to the engine's
/// shared `tool_specs` (every profile may script; a profile masks it like any
/// tool via its `tools`/`disallowed_tools` allowlist — #116).
pub fn rhai_spec() -> ToolSpec {
    ToolSpec::with_schema(
        RHAI_TOOL,
        "Run a Rhai script (https://rhai.rs) in a capability-sandboxed engine — \
         the way to do multi-step logic in one call instead of shelling out to \
         python/node. No filesystem, network, process, or env access: the only \
         host functions bound are read(path), read(path, offset, limit), \
         glob(pattern), grep(pattern), grep(pattern, path), edit(path, old, new), \
         edit(path, old, new, replace_all), write(path, content) — each routed \
         through the same permission checks as the equivalent tool call, and \
         each returns the tool's text output (throws on denial/failure; catch \
         with try/catch). The script's last expression is returned (serialized); \
         print(...) output is captured. Bounded: max_operations, string/array/map \
         size caps, and a wall-clock timeout (default 5s, max 30s).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "Rhai source. The value of its last expression is returned."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Wall-clock budget in seconds (default 5, max 30)."
                }
            },
            "required": ["script"]
        }),
    )
}

/// Parsed `rhai` tool input.
#[derive(Deserialize)]
struct ScriptInput {
    script: String,
    #[serde(default)]
    timeout: Option<u64>,
}

/// Permission decision for one binding, precomputed once per script run.
#[derive(Clone, Copy)]
enum Decision {
    /// Tool masked out of the session's advertised set (#116) — does not exist.
    Masked,
    /// Tool available; run it under this permission.
    Perm(Permission),
}

/// The per-run binding policy: which host functions are masked out (#116), plus
/// the least-privilege permission chain — the session's own profile, each
/// ancestor (#77), then the config ceiling (#172) — resolved *per call* so an
/// argument-scoped rule (#173) sees the binding's actual input. Built once in the
/// executor loop where the profile state lives, then moved into the script task
/// so the read stays ordered with lifecycle events. The mask is argument-
/// independent so it stays a precomputed set; only the grade is resolved live.
pub struct BindingPolicy {
    masked: HashSet<&'static str>,
    /// Profiles folded least-privilege for each call: `[own, ancestors…, base]`.
    chain: Vec<PermissionProfile>,
}

impl BindingPolicy {
    /// Snapshot each binding's mask and the effective permission chain for
    /// `session`, appending the user config's global ceiling (#172) so the
    /// quintet bindings honor the same `permissions` floor — including its
    /// argument-scoped rules (#173) — as a direct tool call.
    pub fn capture(
        active: &HashMap<SessionId, AgentProfile>,
        guard: &SpawnGuard,
        session: &SessionId,
        base: &PermissionProfile,
    ) -> Self {
        let masked = BINDING_TOOLS
            .into_iter()
            .filter(|tool| tool_masked(active, guard, session, tool))
            .collect();
        let mut chain = permission_chain(active, guard, session);
        chain.push(base.clone());
        BindingPolicy { masked, chain }
    }

    /// Resolve one binding call: masked tools do not exist; otherwise the grade
    /// is the least-privileged across the whole chain for this tool + argument.
    fn decide(&self, tool: &'static str, input: &str) -> Decision {
        if self.masked.contains(tool) {
            return Decision::Masked;
        }
        let arg = permission_arg(tool, input);
        let perm = self.chain.iter().fold(Permission::Allow, |acc, p| {
            min_permission(acc, p.resolve(tool, arg.as_deref()))
        });
        Decision::Perm(perm)
    }
}

/// One host-function invocation crossing the bridge: the tool to run, its JSON
/// input, and the channel the blocking thread parks on for the resolved output
/// (`Ok`) or a denial/rejection message the script sees as a thrown exception
/// (`Err`).
struct BindingCall {
    tool: &'static str,
    input: String,
    reply: oneshot::Sender<Result<String, String>>,
}

/// Orchestrate one `rhai` call: gate the tool itself, then run the script with
/// its bindings resolving permission live. `self_perm` is `rhai`'s own effective
/// permission; `policy` is the per-binding snapshot. Approvals (rhai's own gate
/// and each binding `Ask`) route through the lag-proof [`PendingDecisions`]
/// registry (#156) — register-before-emit per request id, mirroring
/// [`crate::tool_runner`].
#[allow(clippy::too_many_arguments)]
pub async fn run_rhai(
    holly: Holly,
    tools: ToolRegistry,
    policy: BindingPolicy,
    self_perm: Permission,
    session: SessionId,
    request_id: String,
    pending: PendingDecisions,
    input: String,
    stop: Arc<AtomicBool>,
) {
    let parsed: ScriptInput = match serde_json::from_str(&input) {
        Ok(p) => p,
        Err(e) => {
            seam::reply(
                &holly,
                session,
                request_id,
                format!("rhai: invalid input: {e}"),
            )
            .await;
            return;
        }
    };
    let timeout = Duration::from_secs(
        parsed
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    );

    // `rhai`'s own permission gate (Allow/Ask/Deny), like any host tool.
    match self_perm {
        Permission::Deny => {
            let out = format!("tool `{RHAI_TOOL}` denied by permission profile");
            seam::reply(&holly, session, request_id, out).await;
            return;
        }
        Permission::Ask => {
            match await_approval(
                &holly,
                &pending,
                &session,
                &request_id,
                RHAI_TOOL,
                &parsed.script,
            )
            .await
            {
                Approval::Approved => set_state(&holly, &session, AgentState::Thinking),
                Approval::Rejected(reason) => {
                    set_state(&holly, &session, AgentState::Thinking);
                    let out = format!("tool `{RHAI_TOOL}` rejected: {reason}");
                    seam::reply(&holly, session, request_id, out).await;
                    return;
                }
                // Stop unwinds silently: core cancels the turn on the same Stop.
                Approval::Stopped => return,
            }
        }
        Permission::Allow => {}
    }

    // A Stop during a binding approval returns `None` and unwinds silently: core
    // cancels the turn on the same Stop, so no ToolResult is owed.
    if let Some(output) = execute_script(
        &tools,
        &policy,
        &holly,
        &session,
        &request_id,
        &pending,
        parsed.script,
        timeout,
        stop,
    )
    .await
    {
        seam::reply(&holly, session, request_id, output).await;
    }
}

/// Run the engine under `spawn_blocking` and service its binding calls on this
/// async task until it finishes. Returns the tool output, or `None` if a `Stop`
/// arrived mid-script (the turn is being cancelled, so no reply is owed).
#[allow(clippy::too_many_arguments)]
async fn execute_script(
    tools: &ToolRegistry,
    policy: &BindingPolicy,
    holly: &Holly,
    session: &SessionId,
    request_id: &str,
    pending: &PendingDecisions,
    script: String,
    timeout: Duration,
    stop: Arc<AtomicBool>,
) -> Option<String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<BindingCall>();
    let prints = Arc::new(Mutex::new(String::new()));
    let engine_prints = prints.clone();
    let start = Instant::now();

    let engine_stop = stop.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut engine = Engine::new_raw();
        configure_engine(&mut engine, timeout, start, engine_prints, engine_stop);
        register_bindings(&mut engine, tx);
        engine.eval::<Dynamic>(&script)
    });

    // Service binding calls until every sender is dropped — which happens only
    // when the engine finishes and the blocking closure returns (dropping the
    // registered functions that hold the senders). `spawn_blocking` cannot be
    // aborted, so the wall-clock timeout is enforced *inside* the engine by the
    // progress callback, not by dropping this task.
    let mut approved: HashSet<&'static str> = HashSet::new();
    let mut stopped = false;
    while let Some(call) = rx.recv().await {
        let (result, was_stopped) = service_binding(
            tools,
            policy,
            holly,
            session,
            request_id,
            pending,
            &mut approved,
            &call,
        )
        .await;
        let _ = call.reply.send(result);
        if was_stopped {
            stopped = true;
        }
    }

    let eval_result = handle.await;
    // A `Stop` for this session (#167): the engine unwound because its progress
    // callback saw the flag. The turn is being cancelled, so no reply is owed —
    // guard here too, since the servicing task's abort may not have landed yet.
    if stopped || stop.load(Ordering::SeqCst) {
        return None;
    }
    let prints = prints.lock().map(|p| p.clone()).unwrap_or_default();
    Some(format_output(prints, eval_result))
}

/// Resolve one binding call per its policy and either run it or refuse. Returns
/// the script-visible result plus whether a `Stop` was seen (which unwinds the
/// whole run). An `Ask` is prompted **once per function per run**: the first
/// `edit` asks, approval covers the rest of that run (per-call prompts in a loop
/// would be noise).
#[allow(clippy::too_many_arguments)]
async fn service_binding(
    tools: &ToolRegistry,
    policy: &BindingPolicy,
    holly: &Holly,
    session: &SessionId,
    request_id: &str,
    pending: &PendingDecisions,
    approved: &mut HashSet<&'static str>,
    call: &BindingCall,
) -> (Result<String, String>, bool) {
    match policy.decide(call.tool, &call.input) {
        Decision::Masked => (
            Err(format!(
                "tool `{}` is not available to this agent (restricted by profile)",
                call.tool
            )),
            false,
        ),
        Decision::Perm(Permission::Deny) => (
            Err(format!("tool `{}` denied by permission profile", call.tool)),
            false,
        ),
        Decision::Perm(Permission::Allow) => (Ok(exec(tools, call).await), false),
        Decision::Perm(Permission::Ask) => {
            if approved.contains(call.tool) {
                return (Ok(exec(tools, call).await), false);
            }
            // Nested approval gets its own request id so the head's Approve/Reject
            // matches this binding, not the outer `rhai` call. The card shows the
            // binding's tool + args; the script source rode the outer approval.
            let bind_rid = format!("{request_id}:rhai:{}", call.tool);
            let card_tool = format!("{} (rhai)", call.tool);
            match await_approval(holly, pending, session, &bind_rid, &card_tool, &call.input).await
            {
                Approval::Approved => {
                    approved.insert(call.tool);
                    set_state(holly, session, AgentState::Thinking);
                    (Ok(exec(tools, call).await), false)
                }
                Approval::Rejected(reason) => {
                    set_state(holly, session, AgentState::Thinking);
                    (
                        Err(format!("tool `{}` rejected: {reason}", call.tool)),
                        false,
                    )
                }
                Approval::Stopped => (Err("rhai run stopped".to_string()), true),
            }
        }
    }
}

/// Execute a delegated host tool and return its text output verbatim (the
/// registry already formats failures as a string, so a binding never hard-errors
/// the run — it surfaces the message to the script). A `rhai` script is a text
/// context, so an image result (#221) collapses to its text parts (empty for an
/// image-only `read`) rather than smuggling base64 into the script.
async fn exec(tools: &ToolRegistry, call: &BindingCall) -> String {
    let content = tools
        .execute(&ToolCall {
            id: format!("rhai:{}", call.tool),
            name: call.tool.to_string(),
            input: call.input.clone(),
            provider_meta: None,
        })
        .await;
    entanglement_core::content_text(&content)
}

/// Outcome of a parked approval round-trip.
enum Approval {
    Approved,
    Rejected(String),
    Stopped,
}

/// Emit a `ToolRequest` and park for the head's decision via the lag-proof
/// [`PendingDecisions`] registry (#156). Shared by `rhai`'s own gate and each
/// binding's `Ask`; registers per `request_id` before emitting so a fast decision
/// routes to this waiter rather than racing a subscription that could lag.
async fn await_approval(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    input: &str,
) -> Approval {
    // Register before emitting so the inbound router can never resolve the
    // decision ahead of this waiter (#156).
    let rx = pending.register(session, request_id);
    // Mint a fresh per-session seq (#157) rather than reusing the `ToolExec` seq.
    holly.emit_for_session(session, |seq| OutEvent::ToolRequest {
        session: session.clone(),
        seq,
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input: input.to_string(),
    });
    set_state(holly, session, AgentState::WaitingApproval);
    match pending::await_decision(rx).await {
        seam::Decision::Approve { .. } => Approval::Approved,
        seam::Decision::Reject { reason } => {
            Approval::Rejected(reason.unwrap_or_else(|| "user".to_string()))
        }
        // `Stop`, a closed inbox, or an unexpected `Answer` all unwind the run.
        seam::Decision::Stop | seam::Decision::Answer { .. } => Approval::Stopped,
    }
}

/// Apply the sandbox: standard (IO-free) package, resource caps, disabled
/// `eval`, the wall-clock progress interrupt, and print capture. `new_raw()`
/// starts with no module resolver, so `import` cannot reach the filesystem.
fn configure_engine(
    engine: &mut Engine,
    timeout: Duration,
    start: Instant,
    prints: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
) {
    engine.register_global_module(StandardPackage::new().as_shared_module());
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_call_levels(MAX_CALL_LEVELS);
    engine.set_max_string_size(MAX_STRING_SIZE);
    engine.set_max_array_size(MAX_ARRAY_SIZE);
    engine.set_max_map_size(MAX_MAP_SIZE);
    // No re-entry into the parser from inside a script.
    engine.disable_symbol("eval");
    engine.on_progress(move |_ops| {
        // A `Stop` for the session trips this flag (#167): terminating from the
        // progress callback yields `ErrorTerminated`, which — unlike a thrown
        // binding error — the script cannot `try`/`catch` and keep running.
        if stop.load(Ordering::Relaxed) {
            Some(Dynamic::from("script stopped".to_string()))
        } else if start.elapsed() >= timeout {
            Some(Dynamic::from(format!(
                "script exceeded the {}s time limit",
                timeout.as_secs()
            )))
        } else {
            None
        }
    });
    engine.on_print(move |text| {
        if let Ok(mut buf) = prints.lock() {
            buf.push_str(text);
            buf.push('\n');
        }
    });
}

/// Bind the host quintet as script functions. Each closure marshals its args to
/// the tool's JSON shape and blocks on the bridge for the resolved output.
fn register_bindings(engine: &mut Engine, tx: UnboundedSender<BindingCall>) {
    let t = tx.clone();
    engine.register_fn("read", move |path: &str| {
        call_binding(&t, "read", serde_json::json!({ "path": path }))
    });
    let t = tx.clone();
    engine.register_fn("read", move |path: &str, offset: i64, limit: i64| {
        call_binding(
            &t,
            "read",
            serde_json::json!({ "path": path, "offset": offset, "limit": limit }),
        )
    });
    let t = tx.clone();
    engine.register_fn("glob", move |pattern: &str| {
        call_binding(&t, "glob", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("grep", move |pattern: &str| {
        call_binding(&t, "grep", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("grep", move |pattern: &str, path: &str| {
        call_binding(
            &t,
            "grep",
            serde_json::json!({ "pattern": pattern, "path": path }),
        )
    });
    let t = tx.clone();
    engine.register_fn("edit", move |path: &str, old: &str, new: &str| {
        call_binding(
            &t,
            "edit",
            serde_json::json!({ "path": path, "oldString": old, "newString": new }),
        )
    });
    let t = tx.clone();
    engine.register_fn(
        "edit",
        move |path: &str, old: &str, new: &str, replace_all: bool| {
            call_binding(
                &t,
                "edit",
                serde_json::json!({
                    "path": path, "oldString": old, "newString": new, "replaceAll": replace_all
                }),
            )
        },
    );
    let t = tx;
    engine.register_fn("write", move |path: &str, content: &str| {
        call_binding(
            &t,
            "write",
            serde_json::json!({ "path": path, "content": content }),
        )
    });
}

/// Send one binding call across the bridge and block for the reply. A denied or
/// rejected call comes back as `Err`, surfaced to the script as a thrown
/// exception it may `try`/`catch`.
fn call_binding(
    tx: &UnboundedSender<BindingCall>,
    tool: &'static str,
    input: serde_json::Value,
) -> Result<String, Box<EvalAltResult>> {
    let (reply, wait) = oneshot::channel();
    tx.send(BindingCall {
        tool,
        input: input.to_string(),
        reply,
    })
    .map_err(|_| runtime_err("rhai host bridge closed"))?;
    match wait.blocking_recv() {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(msg)) => Err(runtime_err(&msg)),
        Err(_) => Err(runtime_err("rhai host bridge dropped")),
    }
}

fn runtime_err(msg: &str) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(msg.into(), Position::NONE))
}

/// Compose the tool output: captured `print` lines, then the serialized return
/// value (or the error), bounded to [`crate::host::MAX_OUTPUT_BYTES`].
fn format_output(
    prints: String,
    eval_result: Result<Result<Dynamic, Box<EvalAltResult>>, tokio::task::JoinError>,
) -> String {
    let mut out = String::new();
    if !prints.is_empty() {
        out.push_str(&prints);
        if !prints.ends_with('\n') {
            out.push('\n');
        }
    }
    match eval_result {
        Ok(Ok(value)) => {
            out.push_str("=> ");
            out.push_str(&serialize_return(&value));
        }
        Ok(Err(e)) => out.push_str(&format!("rhai error: {e}")),
        Err(join) => out.push_str(&format!("rhai error: script task failed: {join}")),
    }
    truncate_output(out)
}

/// Serialize a script's return value. Prefer JSON (arrays/maps/numbers/strings
/// round-trip cleanly); fall back to Rhai's display form for values JSON can't
/// represent. `()` is rendered explicitly so an empty return is unambiguous.
fn serialize_return(value: &Dynamic) -> String {
    if value.is_unit() {
        return "()".to_string();
    }
    match serde_json::to_string(value) {
        Ok(s) => s,
        Err(_) => value.to_string(),
    }
}

fn set_state(holly: &Holly, session: &SessionId, state: AgentState) {
    holly.emit_status(session, state);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a sandboxed engine with no bindings (the bridge senders are dropped
    /// immediately, so any binding call errors) — enough to exercise the sandbox
    /// and resource limits directly.
    fn sandbox_engine(timeout: Duration) -> Engine {
        let mut engine = Engine::new_raw();
        configure_engine(
            &mut engine,
            timeout,
            Instant::now(),
            Arc::new(Mutex::new(String::new())),
            Arc::new(AtomicBool::new(false)),
        );
        engine
    }

    #[test]
    fn arithmetic_and_return_serialize_to_json() {
        let engine = sandbox_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>("let x = 2 + 3; x * 4").unwrap();
        assert_eq!(serialize_return(&v), "20");
    }

    #[test]
    fn array_return_serializes_as_json() {
        let engine = sandbox_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>("[1, 2, 3]").unwrap();
        assert_eq!(serialize_return(&v), "[1,2,3]");
    }

    #[test]
    fn unit_return_renders_explicitly() {
        let engine = sandbox_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>("let _x = 1;").unwrap();
        assert_eq!(serialize_return(&v), "()");
    }

    #[test]
    fn import_is_refused_no_module_resolver() {
        let engine = sandbox_engine(Duration::from_secs(5));
        let err = engine
            .eval::<Dynamic>(r#"import "std" as s; 1"#)
            .unwrap_err();
        // No filesystem module resolver is installed, so `import` cannot escape.
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("module") || msg.contains("import") || msg.contains("resolver"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn no_ambient_file_functions() {
        let engine = sandbox_engine(Duration::from_secs(5));
        // Nothing like `open_file`/`read_file` exists — a bare call is unknown.
        let err = engine
            .eval::<Dynamic>(r#"open_file("/etc/passwd")"#)
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("function"),
            "expected unknown-function error, got: {err}"
        );
    }

    #[test]
    fn operation_limit_terminates_infinite_loop() {
        let engine = sandbox_engine(Duration::from_secs(30));
        // Never returns on its own; the operation cap must kill it.
        let err = engine
            .eval::<Dynamic>("let i = 0; loop { i += 1; }")
            .unwrap_err();
        assert!(
            matches!(*err, EvalAltResult::ErrorTooManyOperations(_)),
            "expected too-many-operations, got: {err}"
        );
    }

    #[test]
    fn timeout_terminates_a_slow_loop() {
        // A sub-second budget: the progress callback interrupts the loop.
        let engine = sandbox_engine(Duration::from_millis(50));
        let err = engine
            .eval::<Dynamic>("let i = 0; loop { i += 1; }")
            .unwrap_err();
        assert!(
            matches!(*err, EvalAltResult::ErrorTerminated(_, _)),
            "expected terminated-by-progress, got: {err}"
        );
    }

    #[test]
    fn stop_flag_terminates_and_cannot_be_caught() {
        // #167: a `Stop` trips the flag; the engine terminates via the progress
        // callback, and a wrapping `try`/`catch` cannot swallow it and continue.
        let stop = Arc::new(AtomicBool::new(true));
        let mut engine = Engine::new_raw();
        configure_engine(
            &mut engine,
            Duration::from_secs(30),
            Instant::now(),
            Arc::new(Mutex::new(String::new())),
            stop,
        );
        let err = engine
            .eval::<Dynamic>(r#"try { let i = 0; loop { i += 1; } } catch(e) { 0 }"#)
            .unwrap_err();
        assert!(
            matches!(*err, EvalAltResult::ErrorTerminated(_, _)),
            "expected terminated-by-stop, got: {err}"
        );
    }

    #[test]
    fn string_size_cap_is_enforced() {
        let engine = sandbox_engine(Duration::from_secs(30));
        // Doubling a string blows past MAX_STRING_SIZE well before it OOMs.
        let err = engine
            .eval::<Dynamic>(r#"let s = "x"; loop { s += s; }"#)
            .unwrap_err();
        assert!(
            matches!(*err, EvalAltResult::ErrorDataTooLarge(_, _))
                || matches!(*err, EvalAltResult::ErrorTooManyOperations(_))
                || matches!(*err, EvalAltResult::ErrorTerminated(_, _)),
            "expected a size/operation/timeout bound, got: {err}"
        );
    }

    #[test]
    fn print_output_is_captured() {
        let prints = Arc::new(Mutex::new(String::new()));
        let mut engine = Engine::new_raw();
        configure_engine(
            &mut engine,
            Duration::from_secs(5),
            Instant::now(),
            prints.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let _ = engine
            .eval::<Dynamic>(r#"print("hello"); print("world");"#)
            .unwrap();
        let captured = prints.lock().unwrap().clone();
        assert_eq!(captured, "hello\nworld\n");
    }

    #[test]
    fn format_output_combines_prints_and_return() {
        let engine = sandbox_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>("40 + 2").unwrap();
        let out = format_output("printed line\n".to_string(), Ok(Ok(v)));
        assert_eq!(out, "printed line\n=> 42");
    }

    #[test]
    fn binding_policy_masks_and_grades() {
        use entanglement_core::{AgentMode, PermissionProfile};

        let profile = AgentProfile {
            name: "readonly".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            permission: PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
            tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        // Allow-all base = the embedded config default: a no-op ceiling.
        let base = PermissionProfile::new(Permission::Allow);
        let policy = BindingPolicy::capture(&active, &guard, &session, &base);

        // `edit` is not in the allowlist → masked.
        assert!(matches!(policy.decide("edit", "{}"), Decision::Masked));
        // `read` survives the mask and is Allow.
        assert!(matches!(
            policy.decide("read", "{}"),
            Decision::Perm(Permission::Allow)
        ));
        // `glob` survives the mask but only its default Ask grade.
        assert!(matches!(
            policy.decide("glob", "{}"),
            Decision::Perm(Permission::Ask)
        ));
    }

    #[test]
    fn binding_policy_honors_config_base_ceiling() {
        use entanglement_core::{AgentMode, PermissionProfile};

        // An allow-all agent, but a config base that forces `read: ask`.
        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow).with("read", Permission::Ask);
        let policy = BindingPolicy::capture(&active, &guard, &session, &base);

        // The base ceiling clamps the `read` binding to Ask despite the agent's
        // allow-all; `write` (base-silent) stays Allow.
        assert!(matches!(
            policy.decide("read", "{}"),
            Decision::Perm(Permission::Ask)
        ));
        assert!(matches!(
            policy.decide("write", "{}"),
            Decision::Perm(Permission::Allow)
        ));
    }

    #[test]
    fn binding_policy_resolves_argument_scoped_rules_per_call() {
        use entanglement_core::{AgentMode, PermissionProfile};

        // Edits ask by default, but edits under `src/` are pre-approved (#173).
        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            permission: PermissionProfile::new(Permission::Ask)
                .with("edit(src/*)", Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy = BindingPolicy::capture(&active, &guard, &session, &base);

        // Same tool, two inputs, two grades — resolved live against the path.
        assert!(matches!(
            policy.decide("edit", r#"{"path":"src/main.rs"}"#),
            Decision::Perm(Permission::Allow)
        ));
        assert!(matches!(
            policy.decide("edit", r#"{"path":"Cargo.toml"}"#),
            Decision::Perm(Permission::Ask)
        ));
    }
}
