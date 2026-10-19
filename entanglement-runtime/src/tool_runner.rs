//! Runtime tool executor. Owns everything about a tool call that is *not* the
//! engine's business: the `Allow | Ask | Deny` permission decision (#59), the
//! approval UX round-trip, and the actual execution against the host-tool
//! [`ToolRegistry`] (#58, ADR-0006/0010).
//!
//! Core emits [`OutEvent::ToolExec`] for **every** host tool and parks on
//! [`InMsg::ToolResult`]; it no longer consults `PermissionProfile`. This task:
//!
//! 1. tracks each session's active [`Agent`] — folded from `SessionStarted`
//!    / `AgentChanged` (ADR-0020) but **self-healed** on every `ToolExec` from the
//!    agent name the event carries (#156), resolved against the
//!    [`AgentCatalog`] handed at startup. That fold is a *lossy* broadcast, so
//!    under burst a dropped lifecycle event would otherwise leave a restricted
//!    session unseen; the self-heal makes the gate authoritative. The grade
//!    itself comes from the session's permission **mode** (ADR-0207 stage 4),
//!    folded from `OutEvent::ModeChanged` the same lossy way — an unseen
//!    session's mode fails *closed* (`Deny`), never the pre-#156 allow-all
//!    fallback that inverted the security posture under overload;
//! 2. on `ToolExec`, resolves the permission for the tool:
//!    - `Deny` → replies `ToolResult("…denied…")` without running it;
//!    - `Allow` → runs it and replies `ToolResult`;
//!    - `Ask` → emits [`OutEvent::ToolRequest`] (the approval prompt) and awaits
//!      the head's `Approve`/`Reject`/`Stop`, then runs-or-refuses accordingly.
//!
//! Each request runs on its own detached task so a slow tool (or a pending
//! approval) can't stall anything else. Core dispatches a model turn's tool
//! calls as a **batch** (#270, ADR-0061): every `ToolExec` of the batch is
//! emitted up front and the turn parks until all results have returned, so
//! multiple executor tasks — and multiple pending approvals — per session are
//! normal. Decision delivery is lag-proof (#156): a parked approval registers a
//! oneshot in [`crate::pending::PendingDecisions`] keyed by `(session,
//! request_id)`, and a single light inbound router (`background.rs`) fans each
//! `Approve`/`Reject`/`AnswerQuestion` to its waiter — replacing the former
//! per-task `broadcast` subscription that could lag and silently drop a decision,
//! parking the request forever.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use entanglement_core::SessionId;

// The interception ladder (issue #451): `Intercept` classifies a `ToolExec`
// by tool name and `route_tool_exec` is the `match` that dispatches each
// route to its handler — split out so this file stays the executor shell
// (`LadderCtx` construction) while the ladder owns everything about which
// handler a tool name reaches.
mod ladder;
use ladder::LadderCtx;

// The executor shell's own seams (issue #712): the lifecycle-folding event
// loop, the background listeners it parks alongside it, the caller-supplied
// policy inputs, the `ToolExecutor` builder, and the deprecated positional
// constructors (#724) — re-exported
// at their pre-split paths.
mod background;
pub(crate) mod call_grader;
pub use call_grader::grade_in_scope;
mod builder;
mod inputs;
mod lifecycle;
mod overlay_warn;
mod wrappers;
pub use builder::{ToolExecutor, ToolExecutorBuilder};
pub use inputs::{DiscoverySurface, EscapeRoot};
#[allow(deprecated)]
pub use wrappers::{
    spawn_tool_executor, spawn_tool_executor_with_hooks, spawn_tool_executor_with_policy,
};

use crate::cancel::{CancelAllOnDrop, CancelRegistry};
use crate::run_limits;

// The dispatch seam (issue #451, ADR-0227): `Dispatcher::run` grades and
// executes one call into an `Outcome`, `Dispatcher::dispatch` replies to core
// with it, and a rhai binding (`crate::script`) takes it back over the
// bridge; `resolve_effective` lives in `dispatch::grade`.
mod dispatch;
pub(crate) use dispatch::grade::resolve_effective;
#[cfg(feature = "rhai")]
pub(crate) use dispatch::{Call, Dispatcher, Outcome, Posture, ScriptCall, Via};

/// The executor body behind [`ToolExecutorBuilder::spawn`] (#724). The
/// per-seam semantics and defaults live on the builder's setters; this only
/// resolves the two defaults that depend on other fields (the mode table and
/// the resolver built over it) and starts the task.
///
/// `agents`/`skills`/`tools` are live handles (#329/#400/#372): every
/// dispatch takes a brief read lock and clones an owned snapshot *before*
/// spawning its detached task (never held across a tool's `.await`), so a
/// reload mid-flight is invisible to an in-progress dispatch.
fn spawn_executor(b: ToolExecutorBuilder) -> tokio::task::JoinHandle<()> {
    let ToolExecutorBuilder {
        holly,
        tools,
        jobs,
        retained,
        scripts,
        agents,
        skills,
        base,
        active,
        perm_modes,
        resolver,
        grants,
        hooks,
        escape_root,
        mode_table,
        plan_files,
        mcp_scopes,
        catalog,
        discovery,
        workdirs,
        http_gate,
        rhai_trusted,
        skill_dirs,
    } = b;
    let mode_table = mode_table.unwrap_or_else(|| {
        Arc::new(crate::mode::ModeTable::builtin().expect("built-in permission modes must parse"))
    });
    let resolver = resolver.unwrap_or_else(|| {
        Arc::new(crate::policy::ModeResolver::new(
            perm_modes.clone(),
            mode_table.clone(),
            tools.clone(),
            base.clone(),
            escape_root.as_ref().map(|er| er.root.clone()),
        ))
    });
    // In-memory only (#747/#775): an executor that didn't ask for persisted
    // grants must never read or write the developer's real `grants.yml`.
    let grants =
        grants.unwrap_or_else(|| Arc::new(crate::policy::DefaultGrantStore::load_from(None)));
    // One gate for the tool that injects and the seam that forces approval.
    #[cfg(feature = "rhai")]
    if let Some(gate) = &http_gate {
        match crate::host::HttpTool::new(gate.clone()) {
            Ok(tool) => tools
                .write()
                .expect("tool registry lock poisoned")
                .register(tool),
            Err(e) => tracing::warn!("rhai http() unavailable: {e:#}"),
        }
    }
    let http_gate = http_gate.unwrap_or_default();
    let DiscoverySurface {
        advertising,
        mcp_avail,
        mcp_active,
        validation,
        http: mcp_http,
    } = discovery;
    let hooks = Arc::new(hooks);
    let sub = holly.subscribe();
    // Subscribe to the inbound fan-out *synchronously*, before this function
    // returns, so a `Prompt`/`Stop` the caller sends right after spawning can't
    // race ahead of the watcher's subscription (the `user_prompt_submit` hook,
    // #199, depends on catching that first prompt).
    let inbound = holly.subscribe_inbound();
    // Same discipline for the budget watcher's own subscription (ADR-0207
    // §11, stage 5c): it's handed off to a task scheduled inside the
    // `tokio::spawn` below, so subscribing there (instead of here) could
    // race a `SessionStarted`/`Usage` broadcast sent right after this
    // function returns and silently miss it.
    let budget_sub = holly.subscribe();
    // Same discipline for the lifecycle hook watcher (#734): a first
    // `SessionStarted` must not race its subscription. Skipped entirely when
    // no observe-only hook point is configured (the norm).
    let lifecycle_subs = hooks
        .has_lifecycle()
        .then(|| (holly.subscribe(), holly.subscribe_inbound()));
    tokio::spawn(async move {
        // Background tasks this executor spawns that must not outlive it (#545):
        // the file-change listener and the decision router below each hold
        // resources (the router a `Holly` clone) that keep the engine's channels
        // open, so a bare detached `tokio::spawn` would block process shutdown
        // forever — nothing else ever aborts them. Parking them in this `JoinSet`
        // instead means they're aborted automatically when *this* task's future
        // drops, whether that's an explicit `.abort()` at shutdown or the loop
        // below breaking on the engine's outbox closing.
        let mut background: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        // Which sessions currently have a loaded skill "active" (#400,
        // ADR-0106; posture-only since ADR-0194 — skills no longer mask
        // tools, this purely tracks whether to emit `OutEvent::SkillActive`'s
        // clearing notice). Set when a `load_skill` call resolves, cleared on
        // the turn's `Done` — a skill's scope is one conversational turn —
        // or when the session ends. Shared with
        // the dispatch seam's execute step (the detached per-call tasks),
        // which sets it after a successful `load_skill`; this loop is
        // the sole writer of the clear path.
        let active_skill: Arc<Mutex<HashSet<SessionId>>> = Arc::new(Mutex::new(HashSet::new()));
        // Per-session, per-turn repeat-denial tracker (ADR-0207 §11, stage
        // 5c): a collapsed-`Ask` denial's second identical `(tool, arg)`
        // this turn parks an approval instead of refusing silently again.
        // Scoped exactly like `active_skill` above — cleared on `Done` and
        // on session end/hibernate.
        let denials = Arc::new(run_limits::DenialTracker::new());
        // The project root `propose_plan` materializes/resolves plan files
        // against (#513): the same canonical root `escape_root` carries when
        // wired (every full head). A wrapper with no escape-root policy (test
        // helpers, a lean embedder) falls back to the process cwd — fine there
        // since none of those callers exercise `propose_plan` against a shared
        // working tree.
        let plan_root: std::path::PathBuf = escape_root
            .as_ref()
            .map(|er| er.root.clone())
            .unwrap_or_else(|| {
                // Canonical, like an escape root: a script `path` is graded
                // root-relative by stripping it (ADR-0228).
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                cwd.canonicalize().unwrap_or(cwd)
            });
        // Answer + timing per launched sub-agent, keyed by its handle (#89).
        // Shared with the detached launch watchers and `poll` tasks (#605).
        let registry = crate::agent_registry::AgentRegistry::default();
        // "Always allow" grants (#174), now a pluggable [`GrantStore`] trait
        // object (#311): the default persists to the managed file, a multi-tenant
        // embedder to its DB. Shared with the per-request dispatch tasks, which
        // record the wider scopes off an `Approve`; the loop reads it (sync
        // `is_granted`) to skip the prompt for an already-granted call.
        // In-flight tool tasks per session (#167). A `Stop` on the inbound
        // fan-out aborts every task registered for that session, so a running
        // `bash`/`call` command or `rhai` script is actually cancelled — core
        // only clears the parked turn state, it never owns the execution.
        let cancels = CancelRegistry::default();
        // Aborts every session's in-flight dispatch/rhai task (#545) when this
        // executor's own task future drops — a `Stop` only cancels one session's
        // registered tasks; process shutdown must cancel all of them, since a
        // long-running `bash`/`call`, a parked approval, or a blocking rhai
        // script each hold a `Holly` clone for as long as they run.
        let _cancel_all_on_drop = CancelAllOnDrop(cancels.clone());
        // Lag-proof decision delivery (#156): parked approvals await a oneshot
        // registered here, and the single inbound router below fans each head
        // decision to its waiter — closing the window where a per-task broadcast
        // park lagged and silently dropped an `Approve`/`Reject`/`Answer`.
        // Every card leaving it is announced to the heads (ADR-0240).
        let pending = crate::pending::PendingDecisions::announcing(holly.clone());
        // Open `ask_user` question content, keyed the same way as `pending`
        // (#515): `pending` alone can't answer `InMsg::ListQuestions`, since a
        // bare `oneshot::Sender` carries no question text. Shared with
        // `run_ask_user` (which owns insert/remove) and the router below
        // (which only reads it for a `ListQuestions` snapshot).
        let open_questions = crate::questions::OpenQuestions::default();
        // The ladder's long-lived shared state (issue #451), built once here
        // by moving the locals above: the lifecycle loop, the background
        // listeners and every ladder handler read them off this one struct.
        let ctx = LadderCtx {
            holly,
            registry,
            retained,
            jobs,
            scripts,
            pending,
            open_questions,
            resolver,
            agents,
            perm_modes,
            mode_table,
            plan_files,
            plan_root,
            cancels,
            tools,
            skills,
            mcp_avail,
            mcp_active,
            mcp_scopes,
            advertising,
            escape_root,
            base,
            grants,
            mcp_http,
            active_skill,
            hooks,
            validation,
            denials,
            catalog,
            workdirs,
            http_gate,
            trust: Arc::new(crate::rhai_trust::Trust::new(rhai_trusted, &skill_dirs)),
        };
        background::spawn_file_change_listener(&mut background, &ctx);
        background::spawn_decision_router(&mut background, inbound, &ctx);
        if let Some((events, inbound)) = lifecycle_subs {
            crate::hooks::lifecycle::spawn_watch(
                &mut background,
                ctx.hooks.clone(),
                events,
                inbound,
            );
        }
        // `max_turns`/`max_duration` enforcement (ADR-0207 §11, stage 5c):
        // its own subscriber, parked in the same `JoinSet` as every other
        // background task here so it's aborted alongside them rather than
        // leaking a `Holly` clone past shutdown.
        background.spawn(crate::run_budget::watch(
            ctx.holly.clone(),
            budget_sub,
            ctx.mode_table.clone(),
        ));
        lifecycle::run(sub, &ctx, &active).await;
    })
}
