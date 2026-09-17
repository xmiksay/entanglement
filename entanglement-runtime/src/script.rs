//! `rhai` — embedded, capability-sandboxed script engine (ADR-0046, amended by
//! ADR-0115 to add exec bindings and ADR-0130 to marshal `workdir`; ADR-0129's
//! active-skill-mask threading was retired by ADR-0194 — skills no longer
//! mask tools).
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
    AgentState, ApprovalScope, Holly, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
    ToolOverlayEntry,
};

use crate::tools::ToolRegistry;
use rhai::packages::{Package, StandardPackage};
use rhai::{Dynamic, Engine, EvalAltResult, Position};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;

// The detached `background: true` path (#637, ADR-0185).
mod background;
// Pure JSON/YAML (de)serialization script functions — no IO, no permission
// check, split out of this (grandfathered over-cap) file.
mod data;
// The model-facing tool spec and its binding reference — what the model is
// told, as opposed to what runs.
mod spec;

pub use spec::rhai_spec;

use crate::host::truncate_head_tail;
use crate::pending::{self, PendingDecisions};
use crate::permission::{
    ancestor_chain, min_permission, overlay_denies, overlay_entry_grade, overlay_grade_entry,
    permission_workdir,
};
use crate::permission_bash::resolve_scoped_bash_aware;
use crate::permission_path::grading_arg;
use crate::policy::PermissionResolver;
use crate::seam;
use crate::subagent::SpawnGuard;
use crate::tool_names::{BINDING_TOOLS, RHAI_TOOL};
use crate::tool_runner::EscapeRoot;

/// Default wall-clock budget for a script, in seconds, when the model omits
/// `timeout`. Clamped to [`MAX_TIMEOUT_SECS`].
const DEFAULT_TIMEOUT_SECS: u64 = 5;
/// Upper bound on a caller-supplied `timeout` (seconds).
const MAX_TIMEOUT_SECS: u64 = 30;
/// Default/max wall-clock budget for a `background: true` script (#637,
/// ADR-0185) — the same regime as a background `bash`/`call` job (#617).
/// Backgrounding is what raising the 30 s cap was deferred *for* (ADR-0161
/// §5); the blocking path keeps its tight bound.
const BG_DEFAULT_TIMEOUT_SECS: u64 = 120;
const BG_MAX_TIMEOUT_SECS: u64 = 600;

// Resource limits (see module docs). Generous enough for real multi-step logic,
// tight enough that a runaway script dies deterministically.
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
    /// Detach and return an `x-` handle instead of the result (#637,
    /// ADR-0185) — the fourth launcher joins the `background` family.
    #[serde(default)]
    background: bool,
}

/// Whether a `rhai` call's input requests the detached path (#637) — read by
/// the tool executor to skip the session-Stop canceller registration before
/// [`run_rhai`] runs: a background script deliberately survives a session
/// `Stop`, exactly as a background `bash`/`call` job does.
pub fn is_background(input: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| v.get("background").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// The per-run binding policy: grades each binding through the session's
/// permission **mode** and the binding tool's declared [`Capability`] — the
/// exact same pluggable [`PermissionResolver`] + ancestor-chain clamp
/// (ADR-0024) a direct tool call resolves through (ADR-0207 stage 4b), so a
/// script's `bash()` grades identically to a model-issued `bash` call
/// instead of a second, `AgentProfile`-chain grading path that could drift
/// from it. Built once in the executor loop where the resolver/chain/overlay
/// state lives, then moved into the script task so the read stays ordered
/// with lifecycle events.
///
/// The `tools`/`disallowed_tools` mask this policy used to also filter
/// bindings through is retired (ADR-0207 §8, "the mask machinery is
/// deleted") — every binding is graded, never withheld by name.
pub struct BindingPolicy {
    /// The ancestor chain (nearest first, ADR-0024) each binding call's grade
    /// is resolved across and clamped least-privilege over — the same chain
    /// [`crate::tool_runner::resolve_effective`] walks for a direct call.
    chain: Vec<SessionId>,
    /// The same pluggable resolver a direct tool call grades through (#311)
    /// — reused here rather than re-deriving the session's mode table, so a
    /// multi-tenant embedder's own resolver governs script bindings too.
    resolver: Arc<dyn PermissionResolver>,
    /// The overlay entry that overrides a binding's grade, keyed by binding
    /// name (#628, closing the ADR-0149 deferral): the nearest
    /// ancestor-chain link with a live overlay **enable** entry for that
    /// binding, same lookup [`overlay_grade_entry`] gives `tool_runner::dispatch`
    /// — so a script's `bash()` under an overlay (its own session's or an
    /// ancestor's) grades the same way a direct `bash` call would, instead
    /// of always falling through to the resolver below.
    overlay: HashMap<&'static str, ToolOverlayEntry>,
    /// Bindings withdrawn by a matching overlay **deny** entry (#634,
    /// ADR-0207 §8 restore) — the same nearest-opinionated-link chain walk
    /// as `overlay` above, via [`crate::permission::overlay_denies`], so
    /// `/disable tool bash` reaches a script's `bash()` binding exactly like
    /// it reaches a direct call.
    denied: HashSet<&'static str>,
    /// The config permission ceiling (#172) an overlay grade still clamps
    /// against — the resolver already clamps its own result against this
    /// same ceiling (`ProfileResolver`), so this is only consulted on the
    /// overlay path, which bypasses the resolver.
    base: PermissionProfile,
    /// The session's permission mode name, captured for the decline message
    /// only (`decide`'s `Deny` case names it, matching `tool_runner::dispatch`'s
    /// wording) — grading itself goes through `resolver`, not this string.
    mode: String,
    /// The project root a path-arg binding's argument is normalized relative
    /// to before matching (#485, ADR-0125) — mirrors `tool_runner::dispatch`'s
    /// use of `grading_arg`. `None` keeps the pre-#485 verbatim match.
    root: Option<PathBuf>,
}

impl BindingPolicy {
    /// Snapshot each binding's overlay grade/withdrawal and the ancestor
    /// chain for `session` — `resolver` is reused as-is (already clamps to
    /// the config ceiling internally), so unlike the pre-4b policy chain
    /// there is no separate chain to fold `base` into.
    pub fn capture(
        guard: &SpawnGuard,
        overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
        session: &SessionId,
        base: &PermissionProfile,
        resolver: Arc<dyn PermissionResolver>,
        mode: String,
        root: Option<&Path>,
    ) -> Self {
        // The session's live tool overlay (#539, ADR-0149) reaches every
        // binding's Ask/Allow grade override (#628) via `overlay` below, and
        // its withdrawal via `denied` (#634) — mirroring
        // `tool_runner::dispatch`'s per-link chain walk instead of resolving
        // only through the mode.
        let session_chain = ancestor_chain(guard, session);
        let overlay: HashMap<&'static str, ToolOverlayEntry> = BINDING_TOOLS
            .into_iter()
            .filter_map(|tool| {
                overlay_grade_entry(overlays, &session_chain, tool).map(|entry| (tool, entry))
            })
            .collect();
        let denied: HashSet<&'static str> = BINDING_TOOLS
            .into_iter()
            .filter(|tool| overlay_denies(overlays, &session_chain, tool))
            .collect();
        BindingPolicy {
            chain: session_chain,
            resolver,
            overlay,
            denied,
            base: base.clone(),
            mode,
            root: root.map(Path::to_path_buf),
        }
    }

    /// Resolve one binding call's grade: a withdrawn binding (#634) declines
    /// flat, ahead of everything else; a tool with an overlay **enable**
    /// grade (#628) resolves that entry clamped to the config ceiling,
    /// replacing the resolver's grade exactly as `tool_runner::dispatch`
    /// does for a direct call; otherwise the grade is the least-privileged
    /// resolver result across the whole ancestor chain for this tool +
    /// argument (+ `workdir` for `exec`/`bash`, #480) — the resolve can hit a
    /// DB for a pluggable resolver, hence `async`. `read_raw` is graded as an
    /// alias of `read` — it is not in `BINDING_TOOLS` at all (never
    /// advertised, see [`crate::host::ReadRawTool`]), so without this alias a
    /// mode restricting `read` would be silently bypassed by a script
    /// reaching for the unlabeled raw path instead. The same alias applies
    /// to the overlay lookup, for the same reason. `glob_json`/`grep_json`
    /// alias `glob`/`grep` identically (ADR-0206) — a structured-output
    /// escape hatch must not be a permission escape hatch.
    async fn decide(&self, tool: &'static str, input: &str) -> Permission {
        let tool = graded_name(tool);
        if self.denied.contains(tool) {
            return Permission::Deny;
        }
        let arg = grading_arg(tool, input, self.root.as_deref());
        let workdir = permission_workdir(tool, input);
        match self.overlay.get(tool) {
            Some(entry) => {
                let overlay_profile = overlay_entry_grade(tool, entry);
                let grade = resolve_scoped_bash_aware(
                    &overlay_profile,
                    tool,
                    arg.as_deref(),
                    workdir.as_deref(),
                );
                min_permission(
                    grade,
                    resolve_scoped_bash_aware(&self.base, tool, arg.as_deref(), workdir.as_deref()),
                )
            }
            None => {
                crate::tool_runner::resolve_effective(&*self.resolver, &self.chain, tool, input)
                    .await
            }
        }
    }
}

/// The mask/grade identity of a binding call: script-facing variant names
/// resolve to the model-facing tool they ride on — `read_raw` → `read`
/// (ADR-0098) and `glob_json`/`grep_json` → `glob`/`grep` (ADR-0206). A
/// variant is never advertised as its own tool, so a profile can only mean
/// the alias target when it masks or grades its family; the mapping lives
/// here (not in `BINDING_TOOLS`) because the *bridge* still dispatches the
/// literal variant name — [`crate::host::GlobJsonTool`] etc. — this is
/// purely the policy identity.
fn graded_name(tool: &'static str) -> &'static str {
    match tool {
        "read_raw" => "read",
        "glob_json" => "glob",
        "grep_json" => "grep",
        other => other,
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
    scripts: crate::script_ops::ScriptRegistry,
) {
    let parsed: ScriptInput = match serde_json::from_str(&input) {
        Ok(p) => p,
        Err(e) => {
            seam::reply(
                &holly,
                session,
                request_id,
                format!("rhai: invalid input: {e}"),
                true,
            )
            .await;
            return;
        }
    };
    // A background script gets the `bash`/`call` background regime (#637);
    // the blocking path keeps its tight ADR-0046 bound.
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

    // `rhai`'s own permission gate (Allow/Ask/Deny), like any host tool.
    match self_perm {
        Permission::Deny => {
            let out = format!("tool `{RHAI_TOOL}` denied by permission profile");
            seam::reply(&holly, session, request_id, out, true).await;
            return;
        }
        Permission::Ask => {
            // The launch gate runs inside the live turn even for a background
            // script (the turn is parked on this very ToolExec), so the state
            // transitions stay on (`detached: false`).
            match await_approval(
                &holly,
                &pending,
                &session,
                &request_id,
                RHAI_TOOL,
                &parsed.script,
                false,
            )
            .await
            {
                Approval::Approved(_) => set_state(&holly, &session, AgentState::Thinking),
                Approval::Rejected(reason) => {
                    set_state(&holly, &session, AgentState::Thinking);
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

    // The launch is the graded decision (ADR-0161 §3): the gate above already
    // ran, so a background script detaches only after Allow/approval.
    if parsed.background {
        background::run_background(
            holly,
            tools,
            policy,
            escape_root,
            session,
            request_id,
            pending,
            parsed.script,
            timeout,
            stop,
            scripts,
        )
        .await;
        return;
    }

    // A Stop during a binding approval returns `None` and unwinds silently: core
    // cancels the turn on the same Stop, so no ToolResult is owed.
    if let Some((output, is_error)) = execute_script(
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
        seam::reply(&holly, session, request_id, output, is_error).await;
    }
}

/// Run the engine under `spawn_blocking` and service its binding calls on this
/// async task until it finishes. Returns the tool output plus whether the
/// script itself errored (a thrown/uncaught Rhai exception or a panicked
/// `spawn_blocking` join, #636/ADR-0176 — distinct from an individual binding's
/// denial, which the script sees as a catchable exception and may recover
/// from), or `None` if a `Stop` arrived mid-script (the turn is being
/// cancelled, so no reply is owed).
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
) -> Option<(String, bool)> {
    let (tx, rx) = mpsc::unbounded_channel::<BindingCall>();
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
        configure_engine(
            &mut engine,
            timeout,
            start,
            move |text| {
                if let Ok(mut buf) = engine_prints.lock() {
                    buf.push_str(text);
                    buf.push('\n');
                }
            },
            engine_stop,
            // The blocking path reports a deadline through the eval error text
            // (`format_output`); only the background path needs the flag.
            Arc::new(AtomicBool::new(false)),
        );
        register_bindings(&mut engine, tx, bash_enabled, start, timeout);
        data::register_data_functions(&mut engine);
        engine.eval::<Dynamic>(&script)
    });

    let stopped = service_bindings(
        rx,
        tools,
        policy,
        escape_root,
        holly,
        session,
        request_id,
        pending,
        false,
    )
    .await;

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

/// Service binding calls until every sender is dropped — which happens only
/// when the engine finishes and the blocking closure returns (dropping the
/// registered functions that hold the senders). `spawn_blocking` cannot be
/// aborted, so the wall-clock timeout is enforced *inside* the engine by the
/// progress callback, not by dropping this task. Returns whether a `Stop`
/// unwound the run. Shared by the blocking path above and the detached
/// background path (#637).
///
/// The approval cache is keyed by `approval_cache_key`, not bare tool name:
/// for `call`/`bash` that key includes the resolved command line, so approving
/// `call(git status)` does not silently pre-clear `call(rm -rf /)` in the same
/// run (#419 fix A). Every other binding keeps the coarser per-function cache
/// (approve one `edit`, cover the rest) — its argument is always a file path
/// already implied by the tool, not an open-ended command line.
#[allow(clippy::too_many_arguments)]
async fn service_bindings(
    mut rx: mpsc::UnboundedReceiver<BindingCall>,
    tools: &ToolRegistry,
    policy: &BindingPolicy,
    escape_root: Option<&EscapeRoot>,
    holly: &Holly,
    session: &SessionId,
    request_id: &str,
    pending: &PendingDecisions,
    detached: bool,
) -> bool {
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
            detached,
        )
        .await;
        let _ = call.reply.send(result);
        if was_stopped {
            stopped = true;
        }
    }
    stopped
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
    detached: bool,
) -> (Result<String, String>, bool) {
    // This binding's own request id (distinct from the outer `rhai` call's):
    // the head's Approve/Reject for a nested `Ask` matches this, not the outer
    // call, and it doubles as the identity a `Once` escape-root grant is bound
    // to (#449) — threaded into `exec` below so the tool that redeems the grant
    // is the exact one that was approved, not a differently-keyed stand-in.
    let bind_rid = format!("{request_id}:rhai:{}", call.tool);

    let perm = match policy.decide(call.tool, &call.input).await {
        // Names the mode, matching `tool_runner::dispatch`'s own decline
        // text (ADR-0207 stage 4b) — covers both an ordinary mode `deny` and
        // an overlay withdrawal (#634), which also resolves to `Deny` here.
        Permission::Deny => {
            return (
                Err(format!(
                    "tool `{}` denied by mode `{}` — use /mode to switch",
                    call.tool, policy.mode
                )),
                false,
            )
        }
        perm => perm,
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
    match await_approval(
        holly,
        pending,
        session,
        &bind_rid,
        &card_tool,
        &card_input,
        detached,
    )
    .await
    {
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
            if !detached {
                set_state(holly, session, AgentState::Thinking);
            }
            (Ok(exec(tools, session, &bind_rid, call).await), false)
        }
        Approval::Rejected(reason) => {
            if !detached {
                set_state(holly, session, AgentState::Thinking);
            }
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
    let execution = tools
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
    entanglement_core::content_text(&execution.content)
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
/// `detached` (#637) suppresses the session-state transition: a background
/// script's Ask arrives outside any live turn, so flipping the session to
/// `WaitingApproval` (and back to `Thinking` on resolution) would strand a
/// stale status on an otherwise idle session — the `ToolRequest` event alone
/// carries the prompt to the head.
#[allow(clippy::too_many_arguments)]
async fn await_approval(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    input: &str,
    detached: bool,
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
    if !detached {
        set_state(holly, session, AgentState::WaitingApproval);
    }
    match pending::await_decision(rx).await {
        seam::Decision::Approve { scope } => Approval::Approved(scope),
        seam::Decision::Reject { reason } => {
            Approval::Rejected(reason.unwrap_or_else(|| "user".to_string()))
        }
        // `Stop`, a closed inbox, or an unexpected `Answer`/`Retract`/`Replace`
        // (the latter two `ask_user`-only, #515) all unwind the run.
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => Approval::Stopped,
    }
}

/// Apply the sandbox: standard (IO-free) package, resource caps, disabled
/// `eval`, the wall-clock progress interrupt, and print capture. `new_raw()`
/// starts with no module resolver, so `import` cannot reach the filesystem.
/// `on_print` receives each `print` line (no trailing newline) — the blocking
/// path buffers them for the final result, the background path (#637) streams
/// them into its registry entry. `timed_out` is set when the deadline branch
/// fires, so the background path can distinguish a timeout from an ordinary
/// script error without parsing the eval error text.
fn configure_engine(
    engine: &mut Engine,
    timeout: Duration,
    start: Instant,
    on_print: impl Fn(&str) + Send + Sync + 'static,
    stop: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
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
            timed_out.store(true, Ordering::Relaxed);
            Some(Dynamic::from(format!(
                "script exceeded the {}s time limit",
                timeout.as_secs()
            )))
        } else {
            None
        }
    });
    engine.on_print(move |text| on_print(text));
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
/// `bash` is registered only when the host `bash` tool itself is in the
/// registry the script delegates through — off, `bash(...)` is an
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
    // Structured search bindings (ADR-0206): same permission grading and
    // same underlying walk/scan as `glob`/`grep` (dispatched to the
    // script-facing variant tools), but the result arrives as a parsed Rhai
    // value — `#{files: […]}` / `#{matches: […], notices: […]}` — not a
    // newline-joined string. The prose tools' text is model-facing; a script
    // consuming it had to hand-parse, and Rhai's string indexing (chars, not
    // lines) turned every mistake into a wrong-but-not-error result.
    let t = tx.clone();
    engine.register_fn("glob_json", move |pattern: &str| {
        call_binding_dynamic(&t, "glob_json", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("glob_json", move |pattern: &str, exclude: rhai::Array| {
        call_binding_dynamic(
            &t,
            "glob_json",
            serde_json::json!({
                "pattern": pattern,
                "exclude": exclude.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
            }),
        )
    });
    let t = tx.clone();
    engine.register_fn("grep_json", move |pattern: &str| {
        call_binding_dynamic(&t, "grep_json", serde_json::json!({ "pattern": pattern }))
    });
    let t = tx.clone();
    engine.register_fn("grep_json", move |pattern: &str, path: &str| {
        call_binding_dynamic(
            &t,
            "grep_json",
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

/// [`call_binding`] for the structured search variants (ADR-0206): the tool
/// returns a JSON document, so the reply is parsed into a Rhai value here —
/// `glob_json` yields `#{files: […], notices: […]}`, `grep_json` a
/// `#{matches: […], notices: […]}` map — instead of passing the raw string
/// through. Shares the bridge and permission path exactly (`graded_name`
/// handles the policy identity); only the return shape differs. A reply that
/// fails to parse is a binding failure (catchable with `try`/`catch`), not a
/// string the script would have to defensively parse itself.
fn call_binding_dynamic(
    tx: &UnboundedSender<BindingCall>,
    tool: &'static str,
    input: serde_json::Value,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let text = call_binding(tx, tool, input)?;
    rhai::serde::to_dynamic(
        &serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| runtime_err(&format!("binding `{tool}` returned malformed JSON: {e}")))?,
    )
    .map_err(|e| {
        runtime_err(&format!(
            "binding `{tool}` result not Rhai-representable: {e}"
        ))
    })
}

/// Compose the tool output: captured `print` lines, then the serialized return
/// value (or the error), bounded to [`crate::host::MAX_OUTPUT_BYTES`] with a
/// head+tail split (#622) — the return value/error is the load-bearing part at
/// the end, the same shape `bash`/`call`/`agent` now share instead of the
/// head-only truncation this used to get.
fn format_output(
    prints: String,
    eval_result: Result<Result<Dynamic, Box<EvalAltResult>>, tokio::task::JoinError>,
) -> (String, bool) {
    let mut out = String::new();
    if !prints.is_empty() {
        out.push_str(&prints);
        if !prints.ends_with('\n') {
            out.push('\n');
        }
    }
    let (line, is_error) = result_line(eval_result);
    out.push_str(&line);
    (truncate_head_tail(out), is_error)
}

/// The final `=> <value>` / `rhai error: …` line for an eval outcome — shared
/// by the blocking composition above and the background path (#637), which
/// streams prints as they happen and appends only this line at finish.
///
/// A script that named a function/variable the engine does not bind gets the
/// binding reference appended ([`spec::binding_hint`]): that is the "guessed
/// the wrong binding" failure, and the catalogue in the result lets the model
/// self-correct on its next call instead of guessing again. It rides the tail
/// of the output, which survives `truncate_head_tail`.
fn result_line(
    eval_result: Result<Result<Dynamic, Box<EvalAltResult>>, tokio::task::JoinError>,
) -> (String, bool) {
    match eval_result {
        Ok(Ok(value)) => (format!("=> {}", serialize_return(&value)), false),
        Ok(Err(e)) => {
            let hint = spec::binding_hint(&e).unwrap_or_default();
            (format!("rhai error: {e}{hint}"), true)
        }
        Err(join) => (format!("rhai error: script task failed: {join}"), true),
    }
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
    use std::sync::RwLock;

    use crate::capability::Capability;
    use crate::mode::{Limits, Mode, ModeTable, Rules};
    use crate::policy::ProfileResolver;
    use crate::tools::{SharedRegistry, Tool};

    /// Test helper (ADR-0207 stage 4b): build a `BindingPolicy` graded
    /// through a single-mode `ProfileResolver` — the successor to the old
    /// `AgentProfile`-chain fixture every `binding_policy_*` test below used
    /// before this stage. `entries` carries the same `tool`/`tool(arg)`/
    /// `tool{workdir}` rule-key grammar the retired `PermissionProfile`
    /// fixtures used (`Rules::from_lists` sorts them into the mode's
    /// grade-keyed lists), so each test's rule strings carry over unchanged.
    /// An empty registry is fine here: every rule below names a literal tool,
    /// never a bare capability class, so `Mode::resolve` never needs a real
    /// `Tool::capabilities()` to match one.
    fn policy_for(
        session: &SessionId,
        guard: &SpawnGuard,
        overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
        default: Permission,
        entries: &[(&str, Permission)],
        base: &PermissionProfile,
    ) -> BindingPolicy {
        let resolver = resolver_for(std::slice::from_ref(session), default, entries, base);
        BindingPolicy::capture(
            guard,
            overlays,
            session,
            base,
            resolver,
            "test".to_string(),
            None,
        )
    }

    /// A minimal stub for the capability-class-named tools (`read`/`write`) a
    /// bare mode rule key of the same spelling resolves to a *class*, never
    /// the literal tool (`mode::rules`'s `capability_class` doc) — so a test
    /// rule like `"read": Allow` only matches when something in the registry
    /// actually declares `Capability::Read` for a tool named `read`. Real
    /// host tools do; this stands in for them without pulling in the real
    /// `ReadTool`'s root-containment machinery these tests don't need.
    struct StubTool {
        name: &'static str,
        capability: Capability,
    }

    #[async_trait::async_trait]
    impl Tool for StubTool {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed(self.name)
        }
        async fn run(&self, input: &str) -> anyhow::Result<String> {
            Ok(input.to_string())
        }
        fn capabilities(&self) -> &'static [Capability] {
            match self.capability {
                Capability::Read => &[Capability::Read],
                Capability::Write => &[Capability::Write],
                Capability::Exec => &[Capability::Exec],
                Capability::Plan => &[Capability::Plan],
                Capability::Control => &[Capability::Control],
            }
        }
    }

    /// The resolver half of [`policy_for`], split out so a test that needs
    /// more than one [`BindingPolicy`] against the *same* mode/resolver — the
    /// overlay-reaches-a-child test below — can call
    /// [`BindingPolicy::capture`] itself once per session, sharing one
    /// resolver bound to every `sessions` entry.
    fn resolver_for(
        sessions: &[SessionId],
        default: Permission,
        entries: &[(&str, Permission)],
        base: &PermissionProfile,
    ) -> Arc<dyn PermissionResolver> {
        let mut allow = Vec::new();
        let mut deny = Vec::new();
        let mut prompt = Vec::new();
        for (key, grade) in entries {
            match grade {
                Permission::Allow => allow.push(key.to_string()),
                Permission::Deny => deny.push(key.to_string()),
                Permission::Ask => prompt.push(key.to_string()),
            }
        }
        let mode = Mode {
            name: "test".to_string(),
            default,
            rules: Rules::from_lists(&deny, &allow, &prompt),
            limits: Limits::default(),
            sandbox: None,
        };
        let modes = Arc::new(Mutex::new(
            sessions
                .iter()
                .map(|s| (s.clone(), "test".to_string()))
                .collect::<HashMap<_, _>>(),
        ));
        let table = Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"));
        let mut registry = ToolRegistry::new();
        registry.register(StubTool {
            name: "read",
            capability: Capability::Read,
        });
        registry.register(StubTool {
            name: "write",
            capability: Capability::Write,
        });
        let registry: SharedRegistry = Arc::new(RwLock::new(registry));
        Arc::new(ProfileResolver::new(
            modes,
            table,
            registry,
            base.clone(),
            None,
        ))
    }

    /// Build a sandboxed engine with no bindings (the bridge senders are dropped
    /// immediately, so any binding call errors) — enough to exercise the sandbox
    /// and resource limits directly.
    fn sandbox_engine(timeout: Duration) -> Engine {
        let mut engine = Engine::new_raw();
        configure_engine(
            &mut engine,
            timeout,
            Instant::now(),
            |_text| {},
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        engine
    }

    /// A sandboxed engine with the pure JSON/YAML functions registered (no host
    /// bindings — those need the async bridge, irrelevant to these tests).
    fn data_engine(timeout: Duration) -> Engine {
        let mut engine = sandbox_engine(timeout);
        data::register_data_functions(&mut engine);
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
            |_text| {},
            stop,
            Arc::new(AtomicBool::new(false)),
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
        let sink = prints.clone();
        let mut engine = Engine::new_raw();
        configure_engine(
            &mut engine,
            Duration::from_secs(5),
            Instant::now(),
            move |text| {
                if let Ok(mut buf) = sink.lock() {
                    buf.push_str(text);
                    buf.push('\n');
                }
            },
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let _ = engine
            .eval::<Dynamic>(r#"print("hello"); print("world");"#)
            .unwrap();
        let captured = prints.lock().unwrap().clone();
        assert_eq!(captured, "hello\nworld\n");
    }

    /// #622: oversized output keeps a head + tail slice (not head-only), so a
    /// return value/error at the end survives truncation.
    #[test]
    fn format_output_oversized_keeps_head_and_tail() {
        use crate::host::MAX_OUTPUT_BYTES;
        let prints = format!("HEAD_MARKER{}", "x".repeat(MAX_OUTPUT_BYTES * 2));
        let engine = sandbox_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>(r#""TAIL_MARKER""#).unwrap();
        let (out, is_error) = format_output(prints, Ok(Ok(v)));
        assert!(out.starts_with("HEAD_MARKER"), "head lost: {out}");
        assert!(out.contains("TAIL_MARKER"), "tail lost: {out}");
        assert!(out.contains("omitted from the middle"), "got: {out}");
        assert!(!is_error);
    }

    #[test]
    fn format_output_combines_prints_and_return() {
        let engine = sandbox_engine(Duration::from_secs(5));
        let v = engine.eval::<Dynamic>("40 + 2").unwrap();
        let (out, is_error) = format_output("printed line\n".to_string(), Ok(Ok(v)));
        assert_eq!(out, "printed line\n=> 42");
        assert!(!is_error);
    }

    #[test]
    fn format_output_flags_a_script_error() {
        let engine = sandbox_engine(Duration::from_secs(5));
        let err = engine.eval::<Dynamic>("throw \"boom\"").unwrap_err();
        let (out, is_error) = format_output(String::new(), Ok(Err(err)));
        assert!(out.contains("rhai error"), "{out}");
        assert!(is_error);
    }

    /// ADR-0207 §8 ("the mask machinery is deleted"): the retired `tools`
    /// allowlist no longer withholds a binding — `edit` (never named by the
    /// mode's own rules) now falls through to the mode's `default` grade
    /// like any other tool, instead of not existing.
    #[tokio::test]
    async fn binding_policy_grades_every_binding_from_the_mode_only() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        // Allow-all base = the embedded config default: a no-op ceiling.
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Ask,
            &[("read", Permission::Allow)],
            &base,
        );

        // `edit` has no rule of its own — falls through to the mode's
        // `default: Ask`.
        assert_eq!(policy.decide("edit", "{}").await, Permission::Ask);
        // `read` has its own explicit rule.
        assert_eq!(policy.decide("read", "{}").await, Permission::Allow);
        // `glob` has no rule of its own either — same default Ask grade.
        assert_eq!(policy.decide("glob", "{}").await, Permission::Ask);
    }

    /// #628: a live tool overlay's grade override reaches a `rhai` binding,
    /// not just the generic dispatch route — a `bash()` binding under a
    /// session's own `Allow` overlay entry runs without the profile's own
    /// `Ask` default, and a spawned child with no overlay of its own
    /// inherits the grade from its parent's.
    #[tokio::test]
    async fn binding_policy_honors_the_overlay_grade_and_reaches_a_child() {
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let mut guard = SpawnGuard::new();
        guard.record_start(parent.clone(), None);
        guard.record_start(child.clone(), Some(parent.clone()));

        let mut overlays = HashMap::new();
        overlays.insert(
            parent.clone(),
            vec![ToolOverlayEntry {
                pattern: "bash".into(),
                allow: true,
                deny: false,
                arg_pattern: None,
            }],
        );
        let base = PermissionProfile::new(Permission::Allow);
        // The mode alone would ask before running `bash`.
        let resolver = resolver_for(
            &[parent.clone(), child.clone()],
            Permission::Ask,
            &[],
            &base,
        );

        // The overlay session's own binding grades Allow, bypassing the
        // mode's Ask default.
        let parent_policy = BindingPolicy::capture(
            &guard,
            &overlays,
            &parent,
            &base,
            resolver.clone(),
            "test".to_string(),
            None,
        );
        assert_eq!(parent_policy.decide("bash", "{}").await, Permission::Allow);

        // The child has no overlay of its own, but inherits the parent's
        // grade for the same binding.
        let child_policy = BindingPolicy::capture(
            &guard,
            &overlays,
            &child,
            &base,
            resolver,
            "test".to_string(),
            None,
        );
        assert_eq!(child_policy.decide("bash", "{}").await, Permission::Allow);
        // An unrelated binding is untouched by the overlay and still asks.
        assert_eq!(child_policy.decide("edit", "{}").await, Permission::Ask);
    }

    /// ADR-0194: skills no longer mask tools; ADR-0207 §8 retires the agent
    /// mask too — a `BindingPolicy` reflects only the permission chain, so
    /// `write`/`read`/`read_raw`/`edit` all grade identically off the
    /// profile's own allow-all default, whether or not `tools` names them.
    #[tokio::test]
    async fn binding_policy_reflects_only_the_mode() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);

        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Allow,
            &[],
            &base,
        );

        assert_eq!(policy.decide("write", "{}").await, Permission::Allow);
        assert_eq!(policy.decide("read", "{}").await, Permission::Allow);
        // `read_raw` is graded as an alias of `read`.
        assert_eq!(policy.decide("read_raw", "{}").await, Permission::Allow);
        // `edit` has no rule of its own — grades the same allow-all default
        // as the rest.
        assert_eq!(policy.decide("edit", "{}").await, Permission::Allow);
    }

    #[tokio::test]
    async fn binding_policy_honors_config_base_ceiling() {
        // An allow-all mode, but a config base that forces `read: ask`.
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow).with("read", Permission::Ask);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Allow,
            &[],
            &base,
        );

        // The base ceiling clamps the `read` binding to Ask despite the mode's
        // allow-all; `write` (base-silent) stays Allow.
        assert_eq!(policy.decide("read", "{}").await, Permission::Ask);
        assert_eq!(policy.decide("write", "{}").await, Permission::Allow);
    }

    #[tokio::test]
    async fn binding_policy_resolves_argument_scoped_rules_per_call() {
        // Edits ask by default, but edits under `src/` are pre-approved (#173).
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Ask,
            &[("edit(src/*)", Permission::Allow)],
            &base,
        );

        // Same tool, two inputs, two grades — resolved live against the path.
        assert_eq!(
            policy.decide("edit", r#"{"path":"src/main.rs"}"#).await,
            Permission::Allow
        );
        assert_eq!(
            policy.decide("edit", r#"{"path":"Cargo.toml"}"#).await,
            Permission::Ask
        );
    }

    /// ADR-0197: a rhai `bash()` binding grades a compound pipeline
    /// per-segment too, not as one full-string glob match — mirroring
    /// `tool_runner::dispatch`'s direct-call behavior for the same command.
    #[tokio::test]
    async fn binding_policy_grades_compound_bash_per_segment() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Ask,
            &[
                ("bash(find *)", Permission::Allow),
                ("bash(grep *)", Permission::Allow),
            ],
            &base,
        );

        // Every segment matches an Allow rule — the whole pipeline is allowed.
        assert_eq!(
            policy
                .decide("bash", r#"{"command":"find . | grep x"}"#)
                .await,
            Permission::Allow
        );
        // `rm` has no rule — the compound falls through to `Ask`, not the
        // over-match a full-string `bash(find *)` glob would have produced.
        assert_eq!(
            policy
                .decide("bash", r#"{"command":"find . && rm -rf /tmp/x"}"#)
                .await,
            Permission::Ask
        );
    }

    /// ADR-0207 §4: `bash`/`call` are two spellings of the same `Exec`
    /// capability and share **one** rule set now — a rule written for either
    /// grades both, superseding the pre-stage-4b `AgentProfile` world where
    /// each carried its own independent rule namespace.
    #[tokio::test]
    async fn binding_policy_grades_call_and_bash_from_one_shared_rule() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Ask,
            &[("bash", Permission::Deny)],
            &base,
        );

        // A rule written for `bash` denies a `call` binding too.
        assert_eq!(policy.decide("call", "{}").await, Permission::Deny);
        assert_eq!(policy.decide("bash", "{}").await, Permission::Deny);
    }

    /// ADR-0207 §8: the retired `tools` allowlist omitting `call`/`bash` no
    /// longer withholds either binding — both still grade through the
    /// mode's own (here, allow-all) rules.
    #[tokio::test]
    async fn binding_policy_no_longer_masks_call_and_bash_when_unruled() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Allow,
            &[],
            &base,
        );

        assert_eq!(policy.decide("call", "{}").await, Permission::Allow);
        assert_eq!(policy.decide("bash", "{}").await, Permission::Allow);
    }

    /// #419: an arg-scoped `call(git *): allow` rule under a `default: ask`
    /// mode pre-clears `git` invocations while everything else still asks —
    /// mirrors the existing `bash(git *)` coverage in `permission.rs`.
    #[tokio::test]
    async fn binding_policy_resolves_call_arg_scoped_git_rule() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Ask,
            &[("call(git *)", Permission::Allow)],
            &base,
        );

        assert_eq!(
            policy
                .decide("call", r#"{"command":"git","args":["status"]}"#)
                .await,
            Permission::Allow
        );
        assert_eq!(
            policy
                .decide("call", r#"{"command":"rm","args":["-rf","/"]}"#)
                .await,
            Permission::Ask
        );
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
    /// mode's default) when the call carries none, exactly like a direct
    /// tool call with no `workdir` argument.
    #[tokio::test]
    async fn binding_policy_resolves_workdir_scoped_bash_rule() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(
            &session,
            &guard,
            &HashMap::new(),
            Permission::Allow,
            &[("bash{/tmp/*}", Permission::Deny)],
            &base,
        );

        assert_eq!(
            policy
                .decide("bash", r#"{"command":"ls","workdir":"/tmp/scratch"}"#)
                .await,
            Permission::Deny
        );
        assert_eq!(
            policy
                .decide("bash", r#"{"command":"ls","workdir":"/home/x"}"#)
                .await,
            Permission::Allow
        );
        // No `workdir` marshalled at all: the rule never matches, same as
        // today's behavior before this call carried the field.
        assert_eq!(
            policy.decide("bash", r#"{"command":"ls"}"#).await,
            Permission::Allow
        );
    }

    /// #634 (gap 2 of ADR-0207 stage 4b): an overlay **deny** entry reaches
    /// a script binding exactly like it reaches a direct call — restored
    /// alongside the generic dispatch path's own deny fix, since a script is
    /// otherwise a live escape hatch around `/disable tool`.
    #[tokio::test]
    async fn binding_policy_honors_an_overlay_deny() {
        let session = SessionId::new("s");
        let guard = SpawnGuard::new();
        let mut overlays = HashMap::new();
        overlays.insert(session.clone(), vec![ToolOverlayEntry::deny("bash")]);
        let base = PermissionProfile::new(Permission::Allow);
        let policy = policy_for(&session, &guard, &overlays, Permission::Allow, &[], &base);

        // The mode alone would allow `bash` outright — the overlay deny
        // still wins.
        assert_eq!(policy.decide("bash", "{}").await, Permission::Deny);
        // An unrelated binding is untouched.
        assert_eq!(policy.decide("read", "{}").await, Permission::Allow);
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
