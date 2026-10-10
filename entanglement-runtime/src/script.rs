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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use entanglement_core::{AgentState, Holly, Permission, SessionId};

use crate::tools::ToolRegistry;
use rhai::{Dynamic, Engine};
use serde::Deserialize;
use tokio::sync::mpsc::{self};
use tokio::sync::oneshot;

// The detached `background: true` path (#637, ADR-0185).
mod background;
// The per-run binding grading snapshot (#451/ADR-0207 stage 4c).
mod binding_policy;
// Pure JSON/YAML (de)serialization script functions — no IO, no permission
// check.
mod data;
// The model-facing tool spec and its binding reference — what the model is
// told, as opposed to what runs.
mod spec;
// The sandboxed engine and its registered bindings (the blocking side).
mod bindings;
// Print + return-value composition into the tool output.
mod output;
// The async binding resolver (grading, escape-root gate, approval, exec).
mod service;

pub use binding_policy::BindingPolicy;
pub use spec::rhai_spec;

use bindings::{configure_engine, register_bindings};
use output::{format_output, result_line};
use service::{await_approval, service_bindings, Approval};

use crate::pending::PendingDecisions;
use crate::seam;
use crate::tool_names::RHAI_TOOL;
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
            // Same wording as a binding's refusal below and as
            // `tool_runner::dispatch`'s: one denial, one explanation. It said
            // "denied by permission profile" until now — naming a concept
            // ADR-0207 retired, in the one path the mode migration missed.
            let out = format!(
                "tool `{RHAI_TOOL}` denied by mode `{}` — use /mode to switch",
                policy.mode
            );
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

fn set_state(holly: &Holly, session: &SessionId, state: AgentState) {
    holly.emit_status(session, state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    use entanglement_core::{PermissionProfile, ToolOverlayEntry};

    use super::output::serialize_return;
    use crate::capability::Capability;
    use crate::mode::{Limits, Mode, ModeTable, Rules};
    use crate::policy::{ModeResolver, PermissionResolver};
    use crate::subagent::SpawnGuard;
    use crate::tools::{SharedRegistry, Tool};
    use rhai::EvalAltResult;

    /// Test helper (ADR-0207 stage 4b): build a `BindingPolicy` graded
    /// through a single-mode `ModeResolver` — the successor to the old
    /// `Agent`-chain fixture every `binding_policy_*` test below used
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
            sandbox_network: false,
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
        Arc::new(ModeResolver::new(
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
    /// grades both, superseding the pre-stage-4b `Agent` world where
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
