//! `rhai` — embedded, capability-sandboxed script engine (ADR-0046, amended by
//! ADR-0115 to add exec bindings, ADR-0129 to thread the session's active
//! skill mask into binding resolution, and ADR-0130 to marshal `workdir`).
//!
//! A runtime-owned host tool that runs a [Rhai](https://rhai.rs) script in one
//! tool call — the sanctioned replacement for "shell out to `python3`/`node`
//! with a heredoc". A fresh Rhai engine has **no** filesystem, network, process
//! spawn, or env access: every capability is a Rust function we register, so the
//! sandbox is deny-by-default. The bound capabilities are the root-contained
//! host quintet (`read`/`glob`/`grep`/`edit`/`write`) plus permission-gated
//! process-exec (`call`/`bash`, ADR-0115) — each routed through the **same
//! permission resolution as a model-issued tool call** (#59), `call`/`bash`
//! graded under the Call capability (#418) like their host-tool counterparts.
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
//! A file/exec binding (`read`/`edit`/`write`/`exec`/`bash`) targeting a path or
//! `workdir` outside the project root is gated by the **same** escape-root
//! policy as a direct tool call (ADR-0109, #446): [`service_binding`] forces an
//! `Ask` even when the binding grades `Allow`, shows the same "outside the
//! project root" warning on the approval card, and — on approval — records the
//! grant into the shared `ExtraRootStore` so the delegated host tool's own
//! containment check lets the call through. Previously a script could only ever
//! *ride* a durable grant recorded earlier by a direct call; a first-time escape
//! from inside a script hard-failed with no chance to prompt.
//!
//! A skill loaded via `load_skill` (#400, ADR-0106) narrows the session's tool
//! set for the rest of its turn; [`BindingPolicy::capture`] snapshots that mask
//! (ADR-0129) alongside the agent mask, so a binding excluded by the active
//! skill's `allowed_tools` refuses the same way a direct call to that tool
//! would — a script is not a side channel around a loaded skill's restriction.
//!
//! `exec`/`bash` also accept an optional `workdir` (#480, ADR-0130:
//! `exec(command, args, workdir)` / `bash(command, workdir)`), marshalled into
//! the delegated tool's own `workdir` field. Threading it through means a
//! workdir-scoped permission rule (`tool{pattern}`, #425/ADR-0116) — previously
//! inert for a binding call, since the marshalled input carried no `workdir` at
//! all — now resolves for real (`BindingPolicy::decide`), and the same value
//! feeds the escape-root gate above with no separate wiring.
//!
//! Resource bounds are by construction: `max_operations`, a wall-clock timeout
//! enforced by the progress callback, `max_call_levels`, and string/array/map
//! size caps — a runaway script terminates deterministically with a clear error,
//! never an OOM. `import`/`eval` are disabled so a script cannot pull in modules
//! or re-enter the parser.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use entanglement_core::{
    AgentProfile, AgentState, ApprovalScope, Holly, OutEvent, Permission, PermissionProfile,
    SessionId, ToolCall, ToolSpec,
};

use crate::tools::ToolRegistry;
use rhai::packages::{Package, StandardPackage};
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{Dynamic, Engine, EvalAltResult, Position};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;

use crate::host::truncate_output;
use crate::pending::{self, PendingDecisions};
use crate::permission::{
    min_permission, permission_chain, permission_workdir, skill_masked, tool_masked, ActiveSkill,
};
use crate::permission_path::grading_arg;
use crate::seam;
use crate::subagent::SpawnGuard;
use crate::tool_names::{BINDING_TOOLS, RHAI_TOOL};
use crate::tool_runner::EscapeRoot;

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
         python/node. Rhai's syntax resembles Rust (fn, let) but it is NOT Rust: \
         there is no `use`/`extern crate`, no std library, no crates — only the \
         functions listed below exist for I/O; everything else (strings, arrays, \
         maps, math, loops) is Rhai's own built-in stdlib, already available with \
         no import. No filesystem, network, process, or env access beyond that: \
         the only host functions bound are read(path), read(path, offset, limit) \
         (returns \"{lineno}: {line}\" text — NOT parseable as JSON/YAML), \
         read_raw(path) (exact file content, no line numbers — use this before \
         parse_json/parse_yaml), glob(pattern), grep(pattern), grep(pattern, path), \
         edit(path, old, new), edit(path, old, new, replace_all), write(path, \
         content), exec(command), exec(command, args), exec(command, args, \
         workdir) (argv exec, no shell — graded as the `call` tool; named \
         exec() because `call` is a reserved Rhai keyword for function-pointer \
         invocation), bash(command), bash(command, workdir) (shell exec via \
         sh -c; only callable when the host has bash enabled — otherwise \
         unknown-function) — `workdir` runs the command in that directory and \
         is what a workdir-scoped permission rule (`tool{pattern}`) matches \
         against, same as a direct `call`/`bash` tool call — each routed through \
         the same permission \
         checks as the equivalent tool call (read_raw graded identically to \
         read), and each returns the tool's text output (throws on denial/ \
         failure; catch with try/catch). exec/bash inherit a timeout clamped \
         to this run's own remaining time budget, so a child process cannot \
         outlive the script. Also bound (pure, no IO, no permission check — these \
         transform a value already in the script, not a file): parse_json(str), \
         to_json(value), parse_yaml(str), to_yaml(value) — parse throws on \
         invalid input, so `try { parse_json(x) } catch(e) {...}` doubles as a \
         syntax validator; JSON/YAML null becomes (); an integer outside i64 \
         range silently widens to an approximate float (put large IDs in JSON as \
         strings to avoid this, same convention as JS). Each is also callable as \
         a method, e.g. read_raw(path).parse_json(). The script's last expression \
         is returned (serialized); print(...) output is captured. Bounded: \
         max_operations, string/array/map size caps, and a wall-clock timeout \
         (default 5s, max 30s). \
         Example: let cfg = read_raw(\"config.json\").parse_json(); let out = \"\"; \
         for f in glob(\"*.rs\") { if f.contains(\"test\") { out += f + \"\\n\"; } } out",
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
#[derive(Clone)]
enum Decision {
    /// Tool masked out of the session's advertised set (#116) — does not exist.
    Masked,
    /// Tool excluded by the active skill's `allowed_tools` (#400, #477) — the
    /// `String` is the skill id, for the refusal message.
    SkillMasked(String),
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
///
/// The skill mask (#400, #477) is captured the same way, as a snapshot rather
/// than a live read: `load_skill` is not itself a binding
/// ([`BINDING_TOOLS`] has no entry for it), so nothing inside a running script
/// can change which skill is active — a snapshot at `Intercept::Rhai` entry is
/// exactly as current as a live read would be, and simpler.
pub struct BindingPolicy {
    masked: HashSet<&'static str>,
    /// Bindings excluded by the active skill's `allowed_tools`, mapped to the
    /// skill id that excluded them (#400, #477) — checked after `masked`,
    /// mirroring `tool_runner`'s agent-mask-then-skill-mask ordering for the
    /// generic dispatch route.
    skill_masked: HashMap<&'static str, String>,
    /// Profiles folded least-privilege for each call: `[own, ancestors…, base]`.
    chain: Vec<PermissionProfile>,
    /// The project root a path-arg binding's argument is normalized relative
    /// to before matching (#485, ADR-0125) — mirrors `tool_runner::dispatch`'s
    /// use of `grading_arg`. `None` keeps the pre-#485 verbatim match.
    root: Option<PathBuf>,
}

impl BindingPolicy {
    /// Snapshot each binding's agent mask, active-skill mask, and the effective
    /// permission chain for `session`, appending the user config's global
    /// ceiling (#172) so the quintet bindings honor the same `permissions`
    /// floor — including its argument-scoped rules (#173) — as a direct tool
    /// call. `active_skill` is the same session-keyed map `tool_runner`'s
    /// generic route checks via [`skill_masked`] — pass an empty map where no
    /// skill can be active (tests, or a caller with no skills wired).
    pub fn capture(
        active: &HashMap<SessionId, AgentProfile>,
        guard: &SpawnGuard,
        session: &SessionId,
        base: &PermissionProfile,
        root: Option<&Path>,
        active_skill: &HashMap<SessionId, ActiveSkill>,
    ) -> Self {
        let masked: HashSet<&'static str> = BINDING_TOOLS
            .into_iter()
            .filter(|tool| tool_masked(active, guard, session, tool))
            .collect();
        let skill_masked = BINDING_TOOLS
            .into_iter()
            .filter(|tool| !masked.contains(tool))
            .filter_map(|tool| {
                skill_masked(active_skill, session, tool).map(|skill_id| (tool, skill_id))
            })
            .collect();
        let mut chain = permission_chain(active, guard, session);
        chain.push(base.clone());
        BindingPolicy {
            masked,
            skill_masked,
            chain,
            root: root.map(Path::to_path_buf),
        }
    }

    /// Resolve one binding call: masked tools do not exist; a tool the active
    /// skill excludes is refused next; otherwise the grade is the
    /// least-privileged across the whole chain for this tool + argument (+
    /// `workdir` for `exec`/`bash`, #480). `read_raw` is graded and masked as
    /// an alias of `read` — it is not in `BINDING_TOOLS`/a profile's
    /// `tools`/`disallowed_tools` at all (never advertised, see
    /// [`crate::host::ReadRawTool`]), so without this alias a profile that
    /// restricts `read` would be silently bypassed by a script reaching for
    /// the unlabeled raw path instead. The same alias applies to the skill
    /// mask, for the same reason.
    fn decide(&self, tool: &'static str, input: &str) -> Decision {
        let tool = if tool == "read_raw" { "read" } else { tool };
        if self.masked.contains(tool) {
            return Decision::Masked;
        }
        if let Some(skill_id) = self.skill_masked.get(tool) {
            return Decision::SkillMasked(skill_id.clone());
        }
        let arg = grading_arg(tool, input, self.root.as_deref());
        let workdir = permission_workdir(tool, input);
        let perm = self.chain.iter().fold(Permission::Allow, |acc, p| {
            min_permission(
                acc,
                p.resolve_scoped(tool, arg.as_deref(), workdir.as_deref()),
            )
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
    escape_root: Option<EscapeRoot>,
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
                Approval::Approved(_) => set_state(&holly, &session, AgentState::Thinking),
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
        escape_root.as_ref(),
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
    escape_root: Option<&EscapeRoot>,
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
    // Whether the host `bash` tool is registered at all — computed before the
    // blocking closure moves `tools` out of reach, since a `bool` is `Copy`
    // but `ToolRegistry` is borrowed here with a non-'static lifetime.
    let bash_enabled = tools.contains("bash");

    let engine_stop = stop.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut engine = Engine::new_raw();
        configure_engine(&mut engine, timeout, start, engine_prints, engine_stop);
        register_bindings(&mut engine, tx, bash_enabled, start, timeout);
        register_data_functions(&mut engine);
        engine.eval::<Dynamic>(&script)
    });

    // Service binding calls until every sender is dropped — which happens only
    // when the engine finishes and the blocking closure returns (dropping the
    // registered functions that hold the senders). `spawn_blocking` cannot be
    // aborted, so the wall-clock timeout is enforced *inside* the engine by the
    // progress callback, not by dropping this task.
    //
    // Keyed by `approval_cache_key`, not bare tool name: for `call`/`bash` that
    // key includes the resolved command line, so approving `call(git status)`
    // does not silently pre-clear `call(rm -rf /)` in the same run (#419 fix A).
    // Every other binding keeps the coarser per-function cache (approve one
    // `edit`, cover the rest) — its argument is always a file path already
    // implied by the tool, not an open-ended command line.
    let mut approved: HashSet<String> = HashSet::new();
    let mut stopped = false;
    while let Some(call) = rx.recv().await {
        let (result, was_stopped) = service_binding(
            tools,
            policy,
            escape_root,
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
/// whole run). An `Ask` is prompted **once per cache key per run** — see
/// [`approval_cache_key`]: for most bindings that's once per function (the
/// first `edit` asks, approval covers the rest — per-call prompts in a loop
/// would be noise); for `call`/`bash` it's once per resolved command line
/// (#419 fix A), since a single approved command must not silently clear a
/// different, more dangerous one in the same run.
///
/// Escape-root gate (ADR-0109, #446): mirrors `tool_runner::dispatch`. A path/
/// `workdir` that resolves outside the project root forces an `Ask` even when
/// `policy.decide` graded `Allow`, unless the user already durably granted this
/// exact `(tool, path)` — e.g. via an earlier direct call. Never applies once
/// the grade is `Deny` (refused above, same as `dispatch`). An escaping call
/// bypasses the coarse per-run `approved` cache — that cache exists to avoid
/// re-prompting an ordinary `Ask`, not to authorize a fresh out-of-root target —
/// so it always re-checks the `ExtraRootStore` instead.
#[allow(clippy::too_many_arguments)]
async fn service_binding(
    tools: &ToolRegistry,
    policy: &BindingPolicy,
    escape_root: Option<&EscapeRoot>,
    holly: &Holly,
    session: &SessionId,
    request_id: &str,
    pending: &PendingDecisions,
    approved: &mut HashSet<String>,
    call: &BindingCall,
) -> (Result<String, String>, bool) {
    // This binding's own request id (distinct from the outer `rhai` call's):
    // the head's Approve/Reject for a nested `Ask` matches this, not the outer
    // call, and it doubles as the identity a `Once` escape-root grant is bound
    // to (#449) — threaded into `exec` below so the tool that redeems the grant
    // is the exact one that was approved, not a differently-keyed stand-in.
    let bind_rid = format!("{request_id}:rhai:{}", call.tool);

    let perm = match policy.decide(call.tool, &call.input) {
        Decision::Masked => {
            return (
                Err(format!(
                    "tool `{}` is not available to this agent (restricted by profile)",
                    call.tool
                )),
                false,
            )
        }
        Decision::SkillMasked(skill_id) => {
            return (
                Err(format!(
                    "tool `{}` is not available while skill `{skill_id}` is active \
                     (restricted by its allowed_tools)",
                    call.tool
                )),
                false,
            )
        }
        Decision::Perm(Permission::Deny) => {
            return (
                Err(format!("tool `{}` denied by permission profile", call.tool)),
                false,
            )
        }
        Decision::Perm(perm) => perm,
    };

    let escape = escape_root
        .and_then(|er| er.escaping(call.tool, &call.input).map(|abs| (er, abs)))
        .filter(|(er, abs)| !er.store.is_durably_allowed(call.tool, abs));

    if perm == Permission::Allow && escape.is_none() {
        return (Ok(exec(tools, session, &bind_rid, call).await), false);
    }

    let key = approval_cache_key(call.tool, &call.input, policy.root.as_deref());
    if escape.is_none() && approved.contains(&key) {
        return (Ok(exec(tools, session, &bind_rid, call).await), false);
    }

    // The card shows the binding's tool + args; the script source rode the
    // outer approval. An escaping call also carries the same "outside the
    // project root" warning a direct call's approval card would (ADR-0109) —
    // otherwise the user approves a generic-looking "{tool} (rhai)" prompt with
    // no signal the script is about to reach outside the project.
    let card_tool = format!("{} (rhai)", call.tool);
    let card_input = match &escape {
        Some((_, abs)) => format!(
            "{}\n\n⚠ accesses a path OUTSIDE the project root: {}",
            call.input,
            abs.display()
        ),
        None => call.input.clone(),
    };
    match await_approval(holly, pending, session, &bind_rid, &card_tool, &card_input).await {
        Approval::Approved(scope) => {
            if let Some((er, abs)) = &escape {
                // Record into the same store a direct call's approval would
                // (`tool_runner::await_decision`), so the delegated host tool's
                // own containment check lets this call through. Bound to
                // `bind_rid` (#449) — the same id `exec` below hands the tool as
                // its request id, so a `Once` grant is redeemed by this exact
                // binding call, not a concurrently-running one.
                er.store.record(call.tool, abs, scope, &bind_rid);
            } else {
                approved.insert(key);
            }
            set_state(holly, session, AgentState::Thinking);
            (Ok(exec(tools, session, &bind_rid, call).await), false)
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

/// The `approved` cache key for one binding call (#419 fix A). For `call`/
/// `bash` the key includes the resolved command line (`grading_arg`, same
/// extraction the permission grade itself uses, #485 — a no-op for these two
/// since neither is a path-arg tool, kept for one canonical extraction path)
/// **and** the `workdir` (#480) — a workdir-scoped rule (`tool{pattern}`) can
/// grade the same command differently in two directories, so an approval in
/// one workdir must not silently clear the same command in another. Every
/// other binding keeps the coarser bare-tool-name key (approve one `edit`,
/// cover the rest of the run) — that surface is already the fixed,
/// pre-existing "once per function" behavior this issue leaves unchanged.
fn approval_cache_key(tool: &'static str, input: &str, root: Option<&Path>) -> String {
    match tool {
        "call" | "bash" => {
            let arg = grading_arg(tool, input, root).unwrap_or_default();
            let workdir = permission_workdir(tool, input).unwrap_or_default();
            format!("{tool}:{arg}:{workdir}")
        }
        _ => tool.to_string(),
    }
}

/// Execute a delegated host tool and return its text output verbatim (the
/// registry already formats failures as a string, so a binding never hard-errors
/// the run — it surfaces the message to the script). A `rhai` script is a text
/// context, so an image result (#221) collapses to its text parts (empty for an
/// image-only `read`) rather than smuggling base64 into the script.
/// `request_id` (#449) is this binding's own id (`bind_rid` in
/// [`service_binding`]) — carried as the `ToolCall`'s id so a delegated host
/// tool's `Once` escape-root grant, bound to that same id, is redeemed by this
/// exact call.
async fn exec(
    tools: &ToolRegistry,
    session: &SessionId,
    request_id: &str,
    call: &BindingCall,
) -> String {
    let content = tools
        .execute(
            &ToolCall {
                id: request_id.to_string(),
                name: call.tool.to_string(),
                input: call.input.clone(),
                provider_meta: None,
            },
            session,
        )
        .await;
    entanglement_core::content_text(&content)
}

/// Outcome of a parked approval round-trip. `Approved` carries the scope
/// (#446) — needed to record an escape-root grant at the scope the user chose,
/// mirroring `tool_runner::await_decision`; `rhai`'s own gate ignores it.
enum Approval {
    Approved(ApprovalScope),
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
        seam::Decision::Approve { scope } => Approval::Approved(scope),
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

/// Bind the host quintet plus permission-gated process-exec as script
/// functions (ADR-0115, amending ADR-0046). Each closure marshals its args to
/// the tool's JSON shape and blocks on the bridge for the resolved output.
/// The argv-exec host tool is bound under the script-callable name `exec`,
/// **not** `call`: `call` is a hard-reserved Rhai keyword
/// (`KEYWORD_FN_PTR_CALL`) the interpreter special-cases in
/// `make_function_call` to always mean "invoke this `FnPtr`" — registering a
/// same-named function is silently shadowed (its first argument gets coerced
/// as a function pointer instead of dispatched to ours). The tool name
/// dispatched to the bridge, its permission grade, and its `BINDING_TOOLS`/
/// capability membership all stay the literal `call` (matching the
/// model-facing `call` tool) — only the script-facing identifier differs.
/// `exec`/`bash` additionally stamp a `timeout` derived from this run's own
/// remaining wall-clock budget (`start`/`timeout`, #419 fix B): rhai's
/// `on_progress` interrupt can't reach into a binding call blocked on
/// `blocking_recv`, so the exec tool's own (much longer, up to 600s) timeout
/// would otherwise stand alone as the only bound on an in-flight child.
/// `bash` is registered only when the host `bash` tool itself is
/// (`bash_enabled`, i.e. `ENTANGLEMENT_ENABLE_BASH`) — off, `bash(...)` is an
/// unknown (catchable) script function rather than a graded-then-failing
/// binding. Each also gains a `workdir` overload (#480, ADR-0129:
/// `exec(command, args, workdir)`/`bash(command, workdir)`) that marshals the
/// value into the delegated tool's own `workdir` field — the same field a
/// `tool{pattern}` workdir-scoped rule (#425) and the escape-root gate (#446)
/// both extract, so a script gets identical scoping to a direct tool call.
fn register_bindings(
    engine: &mut Engine,
    tx: UnboundedSender<BindingCall>,
    bash_enabled: bool,
    start: Instant,
    timeout: Duration,
) {
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
    // Raw counterpart of `read` with no line-number prefix — source for
    // parse_json/parse_yaml, since `read`'s "{lineno}: {line}" format isn't
    // valid JSON/YAML. Graded and masked as an alias of `read`
    // (`BindingPolicy::decide`), not a distinct permission surface.
    engine.register_fn("read_raw", move |path: &str| {
        call_binding(&t, "read_raw", serde_json::json!({ "path": path }))
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
    let t = tx.clone();
    engine.register_fn("write", move |path: &str, content: &str| {
        call_binding(
            &t,
            "write",
            serde_json::json!({ "path": path, "content": content }),
        )
    });
    let t = tx.clone();
    engine.register_fn("exec", move |command: &str| {
        call_binding(
            &t,
            "call",
            serde_json::json!({
                "command": command,
                "args": Vec::<String>::new(),
                "timeout": remaining_timeout_secs(start, timeout),
            }),
        )
    });
    let t = tx.clone();
    engine.register_fn("exec", move |command: &str, args: rhai::Array| {
        let args = call_args_to_strings(args)?;
        call_binding(
            &t,
            "call",
            serde_json::json!({ "command": command, "args": args, "timeout": remaining_timeout_secs(start, timeout) }),
        )
    });
    let t = tx.clone();
    // #480: an explicit `workdir` so a workdir-scoped permission rule
    // (`call{pattern}`, #425) and the escape-root gate (#446) both see it —
    // neither fires for a binding call that never marshals the field.
    engine.register_fn(
        "exec",
        move |command: &str, args: rhai::Array, workdir: &str| {
            let args = call_args_to_strings(args)?;
            call_binding(
                &t,
                "call",
                serde_json::json!({
                    "command": command,
                    "args": args,
                    "workdir": workdir,
                    "timeout": remaining_timeout_secs(start, timeout),
                }),
            )
        },
    );
    if bash_enabled {
        let t = tx.clone();
        engine.register_fn("bash", move |command: &str| {
            call_binding(
                &t,
                "bash",
                serde_json::json!({
                    "command": command,
                    "timeout": remaining_timeout_secs(start, timeout),
                }),
            )
        });
        let t = tx;
        engine.register_fn("bash", move |command: &str, workdir: &str| {
            call_binding(
                &t,
                "bash",
                serde_json::json!({
                    "command": command,
                    "workdir": workdir,
                    "timeout": remaining_timeout_secs(start, timeout),
                }),
            )
        });
    }
}

/// Convert a Rhai `args` array (`exec(command, args)`) to argv strings — a
/// non-string element throws a catchable error naming the offending type
/// rather than silently stringifying it (Rhai's `to_string()` on, say, a map
/// would produce nonsense argv).
fn call_args_to_strings(args: rhai::Array) -> Result<Vec<String>, Box<EvalAltResult>> {
    args.into_iter()
        .map(|v| {
            v.into_string()
                .map_err(|ty| runtime_err(&format!("call: args must be strings, got {ty}")))
        })
        .collect()
}

/// Derive a `call`/`bash` binding's `timeout` (whole seconds, minimum 1) from
/// the script's own remaining wall-clock budget, so an in-flight child cannot
/// outlive the run — rhai's `on_progress` interrupt cannot reach a binding
/// call parked in `blocking_recv`, so the bound has to ride along in the
/// marshalled input instead (#419 fix B).
fn remaining_timeout_secs(start: Instant, timeout: Duration) -> u64 {
    timeout.saturating_sub(start.elapsed()).as_secs().max(1)
}

/// Bind pure JSON/YAML (de)serialization functions. Unlike the host quintet
/// these are *not* bindings — no IO, no permission check, no bridge round-trip
/// — since they only transform a value already in the script's own memory
/// (typically the output of `read()`). Built on Rhai's own `serde` bridge
/// (`rhai::serde::{to_dynamic, from_dynamic}`, already enabled via the crate's
/// `serde` feature), so the JSON/YAML-Value <-> Dynamic mapping is Rhai's own
/// tested behavior, not a hand-rolled converter: `null` -> `()`; an integer
/// outside `i64` range silently widens to an approximate `FLOAT` (Rhai's serde
/// serializer falls back i64 -> decimal (off by default) -> float, same as
/// JS's `JSON.parse` — well-formed JSON already encodes such values as strings
/// to avoid exactly this, so scripts should too). Rhai's UFCS means each is
/// also callable as a method, e.g. `read(path).parse_json()`.
fn register_data_functions(engine: &mut Engine) {
    engine.register_fn("parse_json", parse_json);
    engine.register_fn("to_json", to_json);
    engine.register_fn("parse_yaml", parse_yaml);
    engine.register_fn("to_yaml", to_yaml);
}

fn parse_json(text: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| runtime_err(&format!("invalid JSON: {e}")))?;
    to_dynamic(value)
        .map_err(|e| runtime_err(&format!("JSON value not representable in Rhai: {e}")))
}

fn to_json(value: Dynamic) -> Result<String, Box<EvalAltResult>> {
    let json: serde_json::Value = from_dynamic(&value)
        .map_err(|e| runtime_err(&format!("value not JSON-serializable: {e}")))?;
    serde_json::to_string(&json).map_err(|e| runtime_err(&format!("failed to stringify JSON: {e}")))
}

fn parse_yaml(text: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| runtime_err(&format!("invalid YAML: {e}")))?;
    to_dynamic(value)
        .map_err(|e| runtime_err(&format!("YAML value not representable in Rhai: {e}")))
}

fn to_yaml(value: Dynamic) -> Result<String, Box<EvalAltResult>> {
    let yaml: serde_yaml::Value = from_dynamic(&value)
        .map_err(|e| runtime_err(&format!("value not YAML-serializable: {e}")))?;
    serde_yaml::to_string(&yaml).map_err(|e| runtime_err(&format!("failed to stringify YAML: {e}")))
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

    /// A sandboxed engine with the pure JSON/YAML functions registered (no host
    /// bindings — those need the async bridge, irrelevant to these tests).
    fn data_engine(timeout: Duration) -> Engine {
        let mut engine = sandbox_engine(timeout);
        register_data_functions(&mut engine);
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
            provider: None,
            permission: PermissionProfile::new(Permission::Ask).with("read", Permission::Allow),
            tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        // Allow-all base = the embedded config default: a no-op ceiling.
        let base = PermissionProfile::new(Permission::Allow);
        let policy =
            BindingPolicy::capture(&active, &guard, &session, &base, None, &HashMap::new());

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

    /// #477: the active skill's `allowed_tools` mask, checked after the agent
    /// mask, refuses a binding the agent itself would otherwise permit —
    /// `read_raw` shares the alias-to-`read` treatment the agent mask already
    /// gets, and a tool omitted from the agent's own `tools` allowlist stays
    /// `Decision::Masked` (the agent mask wins, not swapped for the skill's).
    #[test]
    fn binding_policy_honors_the_active_skill_mask_after_the_agent_mask() {
        use entanglement_core::{AgentMode, PermissionProfile};

        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: Some(vec!["read".into(), "write".into()]),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);

        let mut active_skill = HashMap::new();
        active_skill.insert(
            session.clone(),
            crate::permission::ActiveSkill {
                skill_id: "restricted".into(),
                allowed_tools: Some(vec!["read".into()]),
            },
        );
        let policy = BindingPolicy::capture(&active, &guard, &session, &base, None, &active_skill);

        // `write` survives the agent mask but is excluded by the skill.
        assert!(matches!(
            policy.decide("write", "{}"),
            Decision::SkillMasked(id) if id == "restricted"
        ));
        // `read` survives both masks and grades normally.
        assert!(matches!(
            policy.decide("read", "{}"),
            Decision::Perm(Permission::Allow)
        ));
        // `read_raw` is graded/masked as an alias of `read` for the skill mask
        // too, same as it already is for the agent mask.
        assert!(matches!(
            policy.decide("read_raw", "{}"),
            Decision::Perm(Permission::Allow)
        ));
        // `edit` is outside the agent's own `tools` allowlist — the agent
        // mask fires first, not the (looser) skill mask.
        assert!(matches!(policy.decide("edit", "{}"), Decision::Masked));
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
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow).with("read", Permission::Ask);
        let policy =
            BindingPolicy::capture(&active, &guard, &session, &base, None, &HashMap::new());

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
            provider: None,
            permission: PermissionProfile::new(Permission::Ask)
                .with("edit(src/*)", Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy =
            BindingPolicy::capture(&active, &guard, &session, &base, None, &HashMap::new());

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

    /// #419: `call`/`bash` are graded through the same Allow/Ask/Deny chain as
    /// the quintet — the Call capability (#418) applies to them identically.
    #[test]
    fn binding_policy_grades_call_and_bash_allow_ask_deny() {
        use entanglement_core::{AgentMode, PermissionProfile};

        let profile = AgentProfile {
            name: "exec".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Ask)
                .with("call", Permission::Allow)
                .with("bash", Permission::Deny),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy =
            BindingPolicy::capture(&active, &guard, &session, &base, None, &HashMap::new());

        assert!(matches!(
            policy.decide("call", "{}"),
            Decision::Perm(Permission::Allow)
        ));
        assert!(matches!(
            policy.decide("bash", "{}"),
            Decision::Perm(Permission::Deny)
        ));
    }

    /// #419: a profile whose `tools` allowlist omits `call` (and `bash`) masks
    /// both bindings out, same as any other tool the #116 mask governs.
    #[test]
    fn binding_policy_masks_call_and_bash_when_omitted_from_tools() {
        use entanglement_core::{AgentMode, PermissionProfile};

        let profile = AgentProfile {
            name: "readonly".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: Some(vec!["read".into()]),
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy =
            BindingPolicy::capture(&active, &guard, &session, &base, None, &HashMap::new());

        assert!(matches!(policy.decide("call", "{}"), Decision::Masked));
        assert!(matches!(policy.decide("bash", "{}"), Decision::Masked));
    }

    /// #419: an arg-scoped `call(git *): allow` rule under a `default: ask`
    /// profile pre-clears `git` invocations while everything else still asks —
    /// mirrors the existing `bash(git *)` coverage in `permission.rs`.
    #[test]
    fn binding_policy_resolves_call_arg_scoped_git_rule() {
        use entanglement_core::{AgentMode, PermissionProfile};

        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Ask)
                .with("call(git *)", Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy =
            BindingPolicy::capture(&active, &guard, &session, &base, None, &HashMap::new());

        assert!(matches!(
            policy.decide("call", r#"{"command":"git","args":["status"]}"#),
            Decision::Perm(Permission::Allow)
        ));
        assert!(matches!(
            policy.decide("call", r#"{"command":"rm","args":["-rf","/"]}"#),
            Decision::Perm(Permission::Ask)
        ));
    }

    /// #419 fix A: the `approved` cache key scopes `call`/`bash` to the
    /// resolved command line, not the bare tool name — approving one command
    /// must not silently clear a different one.
    #[test]
    fn approval_cache_key_scopes_exec_tools_by_command_not_just_tool_name() {
        let a = approval_cache_key("call", r#"{"command":"git","args":["status"]}"#, None);
        let b = approval_cache_key("call", r#"{"command":"rm","args":["-rf","/"]}"#, None);
        assert_ne!(
            a, b,
            "different call commands must get different cache keys"
        );

        let a_again = approval_cache_key("call", r#"{"command":"git","args":["status"]}"#, None);
        assert_eq!(a, a_again, "the same call command reuses its cache key");

        // Every other binding keeps the coarser bare-tool-name key (approve
        // one `edit`, cover the rest of the run) — unchanged by this fix.
        assert_eq!(
            approval_cache_key("edit", r#"{"path":"a.rs"}"#, None),
            approval_cache_key("edit", r#"{"path":"b.rs"}"#, None),
        );
    }

    /// #480: same command, different `workdir` — a workdir-scoped rule can
    /// grade these two calls differently, so an approval in one workdir must
    /// not silently clear the same command in another.
    #[test]
    fn approval_cache_key_scopes_exec_tools_by_workdir_too() {
        let a = approval_cache_key("bash", r#"{"command":"ls","workdir":"/tmp/a"}"#, None);
        let b = approval_cache_key("bash", r#"{"command":"ls","workdir":"/tmp/b"}"#, None);
        assert_ne!(
            a, b,
            "same command, different workdir must get different cache keys"
        );

        let a_again = approval_cache_key("bash", r#"{"command":"ls","workdir":"/tmp/a"}"#, None);
        assert_eq!(a, a_again, "the same command+workdir reuses its cache key");

        // No `workdir` at all is still its own (empty-suffix) key, distinct
        // from either workdir-scoped variant.
        let no_workdir = approval_cache_key("bash", r#"{"command":"ls"}"#, None);
        assert_ne!(no_workdir, a);
        assert_ne!(no_workdir, b);
    }

    /// #480/ADR-0130: a workdir-scoped `bash{/tmp/*}: deny` rule fires for a
    /// binding call that marshals a `workdir` — inert (falls through to the
    /// profile's default) when the call carries none, exactly like a direct
    /// tool call with no `workdir` argument.
    #[test]
    fn binding_policy_resolves_workdir_scoped_bash_rule() {
        use entanglement_core::{AgentMode, PermissionProfile};

        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Allow)
                .with("bash{/tmp/*}", Permission::Deny),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s");
        let mut active = HashMap::new();
        active.insert(session.clone(), profile);
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy =
            BindingPolicy::capture(&active, &guard, &session, &base, None, &HashMap::new());

        assert!(matches!(
            policy.decide("bash", r#"{"command":"ls","workdir":"/tmp/scratch"}"#),
            Decision::Perm(Permission::Deny)
        ));
        assert!(matches!(
            policy.decide("bash", r#"{"command":"ls","workdir":"/home/x"}"#),
            Decision::Perm(Permission::Allow)
        ));
        // No `workdir` marshalled at all: the rule never matches, same as
        // today's behavior before this call carried the field.
        assert!(matches!(
            policy.decide("bash", r#"{"command":"ls"}"#),
            Decision::Perm(Permission::Allow)
        ));
    }

    /// #480: `exec`/`bash`'s new three/two-arg overloads marshal `workdir`
    /// into the dispatched tool's own JSON input; the workdir-less overloads
    /// leave the field out entirely (not `null`), matching a direct tool
    /// call's shape.
    #[test]
    fn exec_and_bash_bindings_marshal_workdir_when_given() {
        let (tx, mut rx) = mpsc::unbounded_channel::<BindingCall>();
        let mut engine = Engine::new_raw();
        register_bindings(
            &mut engine,
            tx,
            true,
            Instant::now(),
            Duration::from_secs(5),
        );

        let captured: Arc<Mutex<Vec<(&'static str, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured2 = captured.clone();
        let responder = std::thread::spawn(move || {
            while let Some(call) = rx.blocking_recv() {
                captured2
                    .lock()
                    .unwrap()
                    .push((call.tool, call.input.clone()));
                let _ = call.reply.send(Ok(String::new()));
            }
        });

        let _ = engine
            .eval::<Dynamic>(
                r#"
                exec("echo", ["hi"]);
                exec("echo", ["hi"], "/tmp/x");
                bash("echo hi");
                bash("echo hi", "/tmp/y");
                "#,
            )
            .unwrap();
        // Dropping the engine drops every closure's `tx` clone, closing the
        // channel so the responder thread's loop ends.
        drop(engine);
        responder.join().unwrap();

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls
            .iter()
            .all(|(tool, _)| *tool == "call" || *tool == "bash"));

        let parse = |i: usize| -> serde_json::Value { serde_json::from_str(&calls[i].1).unwrap() };
        assert!(
            parse(0).get("workdir").is_none(),
            "exec(cmd, args) carries no workdir field"
        );
        assert_eq!(parse(1)["workdir"], "/tmp/x");
        assert!(
            parse(2).get("workdir").is_none(),
            "bash(cmd) carries no workdir field"
        );
        assert_eq!(parse(3)["workdir"], "/tmp/y");
    }

    #[test]
    fn parse_json_round_trips_object_and_array() {
        let engine = data_engine(Duration::from_secs(5));
        let v = engine
            .eval::<Dynamic>(r#"let v = parse_json("{\"a\":1,\"b\":[1,2,3]}"); v["b"][1]"#)
            .unwrap();
        assert_eq!(v.as_int().unwrap(), 2);
    }

    #[test]
    fn parse_json_null_becomes_unit() {
        let engine = data_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>(r#"parse_json("null")"#).unwrap();
        assert!(v.is_unit());
    }

    #[test]
    fn parse_json_throws_on_invalid_input_and_is_catchable() {
        let engine = data_engine(Duration::from_secs(5));
        // A bare `try`/`catch` always evaluates to `()` in Rhai regardless of
        // which branch ran — assign into an outer variable to observe the
        // catch branch actually executed.
        let v = engine
            .eval::<Dynamic>(
                r#"let result = ""; try { parse_json("{not json"); } catch(e) { result = "caught"; } result"#,
            )
            .unwrap();
        assert_eq!(v.into_string().unwrap(), "caught");
    }

    #[test]
    fn parse_json_out_of_i64_range_number_widens_to_float() {
        let engine = data_engine(Duration::from_secs(5));
        // u64::MAX exceeds Rhai's i64 INT range. Rhai's own serde bridge
        // (ser.rs `serialize_u64`) falls back i64 -> decimal (off by default in
        // this build) -> float rather than erroring — same as JS's
        // `JSON.parse`. Not a throw: verified empirically, documented on
        // `register_data_functions` rather than assumed.
        let v = engine
            .eval::<Dynamic>(r#"parse_json("18446744073709551615")"#)
            .unwrap();
        assert!(
            v.is_float(),
            "expected the oversized integer to widen to FLOAT, got: {v:?}"
        );
    }

    #[test]
    fn parse_json_callable_as_method_via_ufcs() {
        let engine = data_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>(r#""[1,2]".parse_json()"#).unwrap();
        assert_eq!(serialize_return(&v), "[1,2]");
    }

    #[test]
    fn to_json_stringifies_a_rhai_value() {
        let engine = data_engine(Duration::from_secs(5));
        let v = engine
            .eval::<Dynamic>(r#"#{a: 1, b: "x"}.to_json()"#)
            .unwrap();
        // Map key order isn't guaranteed — assert on parsed structure, not text.
        let parsed: serde_json::Value = serde_json::from_str(&v.into_string().unwrap()).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], "x");
    }

    #[test]
    fn json_round_trip_is_stable() {
        let engine = data_engine(Duration::from_secs(5));
        let v = engine
            .eval::<Dynamic>(r#"parse_json(to_json(parse_json("[1,2,3]")))"#)
            .unwrap();
        assert_eq!(serialize_return(&v), "[1,2,3]");
    }

    #[test]
    fn parse_yaml_round_trips_and_method_call_works() {
        let engine = data_engine(Duration::from_secs(5));
        let v = engine
            .eval::<Dynamic>(
                r#"let m = "a: 1\nb: two\n".parse_yaml(); m["a"].to_string() + "," + m["b"]"#,
            )
            .unwrap();
        assert_eq!(v.into_string().unwrap(), "1,two");
    }

    #[test]
    fn parse_yaml_throws_on_invalid_input() {
        let engine = data_engine(Duration::from_secs(5));
        // Unbalanced flow-mapping brace — invalid YAML.
        let result = engine.eval::<Dynamic>(r#"parse_yaml("a: [1, 2")"#);
        assert!(result.is_err(), "expected invalid YAML to throw");
    }
}
