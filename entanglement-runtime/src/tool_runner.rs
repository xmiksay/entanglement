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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{Agent, AgentCatalog, Holly, PermissionProfile, SessionId};

// The interception ladder (issue #451): `Intercept` classifies a `ToolExec`
// by tool name and `route_tool_exec` is the `match` that dispatches each
// route to its handler — split out so this file stays the executor shell
// (`LadderCtx` construction) while the ladder owns everything about which
// handler a tool name reaches.
mod ladder;
use ladder::LadderCtx;

// The executor shell's own seams (issue #712): the lifecycle-folding event
// loop, the background listeners it parks alongside it, the caller-supplied
// policy inputs, and the default-policy convenience wrappers — re-exported
// at their pre-split paths.
mod background;
mod inputs;
mod lifecycle;
mod wrappers;
pub use inputs::{DiscoverySurface, EscapeRoot};
pub use wrappers::{spawn_tool_executor, spawn_tool_executor_with_hooks};

use crate::cancel::{CancelAllOnDrop, CancelRegistry};
use crate::hooks::Hooks;
use crate::plan_files::PlanFileRegistry;
use crate::policy::{GrantStore, PermissionResolver};
use crate::run_limits;
use crate::skills::SkillRegistry;
use crate::tool_advertising;
use crate::tools::SharedRegistry;

// The generic tool-call primitives (issue #451) — `dispatch`, the parked-
// approval tail `await_decision`, and the run-against-the-registry step
// `run_and_reply` — split out so this file stays the executor loop shell
// (lifecycle folding + `LadderCtx` construction); `apply_grant`/
// `resolve_effective` live in `dispatch::grade`. Re-exported at their
// pre-split paths (`tool_runner::dispatch`/`apply_grant`/`resolve_effective`)
// since `ladder::graded`, `propose_plan`, and `script::binding_policy` all
// call them by those paths.
mod dispatch;
pub(crate) use dispatch::dispatch;
pub(crate) use dispatch::grade::resolve_effective;
// `apply_grant`'s only caller outside `dispatch` is the rhai route in
// `ladder::graded`, which is feature-gated — so an ungated re-export is an
// unused import in the `--no-default-features` lean build (ADR-0025) and
// `make check-lean` fails it under `-D warnings`.
#[cfg(feature = "rhai")]
pub(crate) use dispatch::grade::apply_grant;

/// Like [`spawn_tool_executor_with_hooks`] but with pluggable policy seams (#311):
/// a [`PermissionResolver`] decides each call's `Allow | Ask | Deny` grade and a
/// [`GrantStore`] persists "always allow" grants, so a multi-tenant embedder can
/// store rules per user in its own DB without forking the executor. `active` is
/// the shared per-session agent map the executor folds lifecycle events into —
/// still driving spawn gating (#119), the sandbox policy, and the `rhai`
/// binding policy, which stay in the ladder on top of the resolver.
/// `perm_modes` is the same fold for the session's permission mode
/// (ADR-0207 stage 4), which the default [`ModeResolver`] grades every
/// call from. The two default wrappers above plug in [`ModeResolver`] +
/// [`DefaultGrantStore`] for the CLI.
///
/// `agents` is behind an `Arc<RwLock<..>>` (#329, not a plain owned
/// [`AgentCatalog`]) so a runtime definitions watcher can swap in a
/// freshly-reloaded registry without restarting this executor — every lookup
/// below takes a brief read lock and clones the hit into the (already-cloning)
/// `active`/mask/spawn-refusal call sites, so a reload mid-flight is invisible
/// to an in-progress dispatch. Core's own copy is untouched either way
/// (ADR-0084): it is baked into `EngineConfig` once at startup and has no
/// live-swap seam.
///
/// `tools` is likewise a [`SharedRegistry`] (#372, ADR-0096, not a plain owned
/// [`ToolRegistry`]) so a live tool-registration change — MCP add/remove (#4) —
/// is visible to this executor without a restart: each dispatch takes a brief
/// read lock and clones an owned snapshot *before* spawning the detached task
/// (never held across a tool's `.await`), mirroring the `agents` pattern
/// above.
///
/// `skills` (#400, ADR-0106) is the same live-reloadable handle
/// `LoadSkillTool` resolves against: after a `load_skill` call succeeds, this
/// executor parses the `skill_id` its result carries and emits
/// `OutEvent::SkillActive` (posture-only since ADR-0194 — skills no longer
/// narrow the session's tool set, so there is no mask to activate here).
///
/// `jobs` (#605) is the same [`crate::host::jobs::JobRegistry`] the caller
/// wires into its `BashTool` — shared so `poll`'s job-handle path reaches the
/// jobs `bash` actually spawned; unlike `tools`/`skills`/`agents` this isn't
/// itself hot-swappable, only cheaply cloned (an `Arc` internally). `retained`
/// (#608) is the same story for
/// [`crate::retained_output::RetainedOutputRegistry`]: the caller's
/// `CallTool` writes a truncated result's full text there, `poll`'s
/// retained-output-handle path reads it back.
///
/// `plan_files` (#513) is the shared [`PlanFileRegistry`] this executor's
/// `propose_plan` dispatch arm and its own `FileChange` listener (spawned
/// in `background.rs`) both read/write. Taken as a param (#627) rather than
/// constructed internally so a caller wiring up the dedicated plans-folder
/// watch ([`crate::plan_watch::spawn_plans_watcher`]) can hand it the exact
/// same instance — the watch's out-of-band notice and this executor's
/// staleness guard must agree on what the agent last knew.
#[allow(clippy::too_many_arguments)]
pub fn spawn_tool_executor_with_policy(
    holly: &Holly,
    tools: SharedRegistry,
    jobs: crate::host::jobs::JobRegistry,
    retained: crate::retained_output::RetainedOutputRegistry,
    // Background `rhai` scripts (#637, ADR-0185) — same shared-instance shape
    // as `jobs`/`retained`: the `rhai` launcher writes, `poll`'s `x-` path and
    // the `ListOperations` router read.
    scripts: crate::script_ops::ScriptRegistry,
    agents: Arc<RwLock<AgentCatalog>>,
    skills: Arc<RwLock<Arc<SkillRegistry>>>,
    base: PermissionProfile,
    active: Arc<Mutex<HashMap<SessionId, Agent>>>,
    // Per-session permission mode (ADR-0207 stage 4), folded from
    // `OutEvent::ModeChanged` the same way `active` folds `AgentChanged` —
    // the shared map the default `resolver` (`ModeResolver`) grades every
    // call from. An unseen session fails closed, mirroring `active`. Named
    // `perm_modes` (not `modes`) to stay distinct from
    // `tool_advertising`'s unrelated session→`ToolAdvertising`-mode map.
    perm_modes: Arc<Mutex<HashMap<SessionId, String>>>,
    resolver: Arc<dyn PermissionResolver>,
    grants: Arc<dyn GrantStore>,
    hooks: Hooks,
    escape_root: Option<EscapeRoot>,
    // The mode table `perm_modes` names resolve against (ADR-0207 §6, stage
    // 5b) — used here only to source a spawn's `max_depth`/`max_agents`
    // bound (`SpawnGuard::try_spawn`) from the session's current mode.
    // Sandbox confinement no longer folds through this executor at all: it
    // reads the identical `perm_modes` map directly via
    // `crate::policy::SandboxConfig`/`ModeSandboxResolver`, wired into
    // `bash`/`call` at registration time, since a mode's sandbox posture
    // needs no lifecycle-event bookkeeping the way the old per-agent
    // ancestor floor did — the whole spawn sub-tree shares one mode.
    mode_table: Arc<crate::mode::ModeTable>,
    // Per-session plan-file staleness tracking (#513), taken as a param
    // (rather than constructed inside, as before #627) so a caller that also
    // wants the dedicated plans-folder watch (`plan_watch::spawn_plans_watcher`)
    // can share the exact same registry instance with it.
    plan_files: Arc<PlanFileRegistry>,
    // Session-keyed per-user MCP scopes (#684): a scoped session's dispatch
    // snapshot has its `mcp__*` namespace replaced by the scope's own tools
    // (lazily connected with the scope's credentials) before `dispatch` runs.
    // `None` — skutter and every in-tree caller — is byte-identical to
    // pre-#684 behavior; only a multi-user embedder constructs an `McpScopes`.
    mcp_scopes: Option<Arc<crate::mcp::McpScopes>>,
    // Tool-advertising resolution inputs (ADR-0196, Phase P1): the user
    // config (its `tool_advertising` tier) and the catalog (the per-model
    // `tool_advertising:` tier). `None` — the convenience wrappers and test
    // callers — keeps the session→mode map recording but always resolving
    // `tool_search` (no catalog entry sets `tool_advertising:` yet and the
    // config key defaults to `null`).
    advertising_inputs: Option<Arc<tool_advertising::AdvertisingInputs>>,
    // `explore`/`describe`'s shared state (#560, ADR-0196 §4), bundled into
    // one struct so this already-large signature doesn't grow by three:
    // the session-mode map + discovered-tool set (also read by the
    // `tool_spec_resolver`/`system_prompt_resolver` closures in `main.rs` —
    // this executor is one of three readers/writers, no longer the map's
    // sole owner as it was in Phase P1) plus the MCP three-state roster
    // `explore` lists. `None` (the convenience wrappers, every test-only
    // caller) falls back to empty/private defaults — `explore` then has
    // nothing MCP to report, and the discovered set is unshared.
    discovery: Option<DiscoverySurface>,
) -> tokio::task::JoinHandle<()> {
    let DiscoverySurface {
        advertising,
        mcp_avail,
        mcp_active,
        validation,
        http: mcp_http,
    } = discovery.unwrap_or_default();
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
    let holly = holly.clone();
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
        // `dispatch`/`await_decision`/`run_and_reply` (the detached per-call
        // tasks), which set it after a successful `load_skill`; this loop is
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
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
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
        let pending = crate::pending::PendingDecisions::default();
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
            // #560 P12, ADR-0207 §12: read off the same `advertising_inputs`
            // every session-start resolution already consults.
            catalog: advertising_inputs
                .as_ref()
                .and_then(|i| i.catalog().cloned()),
        };
        background::spawn_file_change_listener(&mut background, &ctx);
        background::spawn_decision_router(&mut background, inbound, &ctx);
        // `max_turns`/`max_duration` enforcement (ADR-0207 §11, stage 5c):
        // its own subscriber, parked in the same `JoinSet` as every other
        // background task here so it's aborted alongside them rather than
        // leaking a `Holly` clone past shutdown.
        background.spawn(crate::run_budget::watch(
            ctx.holly.clone(),
            budget_sub,
            ctx.mode_table.clone(),
        ));
        lifecycle::run(sub, &ctx, &active, advertising_inputs.as_deref()).await;
    })
}

#[cfg(test)]
mod tests {
    use crate::tool_names::{
        is_non_maskable, DESCRIBE_TOOL, EXPLORE_TOOL, POLL_TOOL, RESPONSES_TOOL_SEARCH_TOOL,
    };

    #[test]
    fn explore_and_describe_are_non_maskable() {
        assert!(is_non_maskable(EXPLORE_TOOL));
        assert!(is_non_maskable(DESCRIBE_TOOL));
        // P7: the wire declares `tool_search` itself whenever a tool is
        // deferred — the model never chose it from an advertised name — so
        // masking it out would strand the round-trip with no way to answer.
        assert!(is_non_maskable(RESPONSES_TOOL_SEARCH_TOOL));
        assert!(
            !is_non_maskable(POLL_TOOL),
            "poll's mask exemption was retired by ADR-0192"
        );
        assert!(!is_non_maskable("bash"));
    }
}
