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
//! request_id)`, and a single light inbound router (spawned below) fans each
//! `Approve`/`Reject`/`AnswerQuestion` to its waiter — replacing the former
//! per-task `broadcast` subscription that could lag and silently drop a decision,
//! parking the request forever.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{
    Agent, AgentCatalog, AgentState, Holly, InMsg, OutEvent, PermissionProfile, SessionId,
};

// The interception ladder (issue #451): `Intercept` classifies a `ToolExec`
// by tool name and `route_tool_exec` is the `match` that dispatches each
// route to its handler — split out so this file stays the executor loop
// shell (lifecycle folding + `LadderCtx` construction) while the ladder
// owns everything about which handler a tool name reaches.
mod ladder;
use ladder::{Intercept, LadderCtx};

use crate::tools::{SharedRegistry, ToolRegistry};
use tokio::sync::broadcast::error::RecvError;

use crate::arg_validate;
use crate::cancel::{CancelAllOnDrop, CancelRegistry};
use crate::hooks::Hooks;
use crate::mcp::{ActiveServers, AvailableMcp};
use crate::mode::ModeTable;
use crate::plan_files::PlanFileRegistry;
use crate::policy::{DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver};
use crate::run_limits;
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_advertising::{self, SharedAdvertisingState};

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

/// Spawn the per-engine tool executor. Subscribes synchronously (so no
/// `ToolExec` emitted before the task is scheduled is missed) and runs until the
/// engine's outbox closes. `agents` is the runtime's copy of the engine's
/// [`AgentCatalog`] — the permission *shape* stays a core type; the runtime
/// only reads it (ADR-0003). `base` is the user config's global permission
/// ceiling (#172): every resolved grade is clamped least-privilege against it.
pub fn spawn_tool_executor(
    holly: &Holly,
    tools: ToolRegistry,
    agents: AgentCatalog,
    base: PermissionProfile,
) -> tokio::task::JoinHandle<()> {
    spawn_tool_executor_with_hooks(holly, tools, agents, base, Hooks::default())
}

/// Wrap a caller's [`SkillRegistry`] for [`spawn_tool_executor_with_policy`]'s
/// `skills` parameter, mirroring [`wrap_agents`]. The convenience wrappers
/// below plug in an empty registry — no `load_skill` mask ever activates for
/// their (~30, test-only) callers, matching their historical no-skill-mask
/// behavior byte-for-byte.
fn wrap_skills(skills: SkillRegistry) -> Arc<RwLock<Arc<SkillRegistry>>> {
    Arc::new(RwLock::new(Arc::new(skills)))
}

/// Wrap a caller's owned [`AgentCatalog`] for [`spawn_tool_executor_with_policy`],
/// which reads it through an `Arc<RwLock<..>>` so a live definitions watcher
/// (#329) can swap it for a fresher one without restarting the executor. The
/// convenience wrappers here keep their historical owned-registry signature for
/// existing callers (and tests) that need no live reload.
fn wrap_agents(agents: AgentCatalog) -> Arc<RwLock<AgentCatalog>> {
    Arc::new(RwLock::new(agents))
}

/// Like [`spawn_tool_executor`] but with user-configured lifecycle hooks (#199,
/// ADR-0066): `pre_tool_use` can veto a generic tool dispatch, `post_tool_use`
/// runs as a side-effect after it, and `user_prompt_submit` fires on every
/// inbound `Prompt`. The no-hook wrapper keeps the historical 4-arg signature for
/// callers (and tests) that need no hooks.
pub fn spawn_tool_executor_with_hooks(
    holly: &Holly,
    tools: ToolRegistry,
    agents: AgentCatalog,
    base: PermissionProfile,
    hooks: Hooks,
) -> tokio::task::JoinHandle<()> {
    // The default single-user policy (#311, ADR-0207 stage 4): the executor
    // folds lifecycle events into `active` (still needed for spawn gating
    // and the `rhai` binding policy) and `modes` (the session→mode map
    // `ModeResolver` grades from — sandboxing reads the identical map via
    // `crate::policy::ModeSandboxResolver`, ADR-0207 §6, wired independently
    // by a caller that wants it; these test-only wrappers don't). "Always
    // allow" grants persist to the managed file.
    let active = Arc::new(Mutex::new(HashMap::new()));
    let modes = Arc::new(Mutex::new(HashMap::new()));
    let mode_table = Arc::new(ModeTable::builtin().expect("built-in permission modes must parse"));
    let shared_tools = tools.shared();
    // No escape-root policy wired here (root: None) — the strict-containment
    // 4-arg wrapper keeps the pre-#485 verbatim arg match (ADR-0125).
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        base.clone(),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    spawn_tool_executor_with_policy(
        holly,
        shared_tools,
        // No job registry shared with an external `BashTool` here — the
        // convenience wrappers' (~30, test-only) callers never wire one up
        // either, so a `poll` of a job id from this executor's own private
        // registry is simply unreachable, matching pre-#605 behavior where
        // these wrappers never registered `bash_output` against a shared one.
        crate::host::jobs::JobRegistry::new(),
        // Same story for retained output (#608): no external `CallTool` shares
        // this private registry either, so a truncated call from one of these
        // wrappers' callers never mints a handle in the first place.
        crate::retained_output::RetainedOutputRegistry::new(),
        // And for background scripts (#637): a private registry is still
        // reachable here — the executor's own `rhai` arm writes it and its
        // `poll` arm reads it back, both inside this one executor.
        crate::script_ops::ScriptRegistry::new(),
        wrap_agents(agents),
        wrap_skills(SkillRegistry::default()),
        base,
        active,
        modes,
        resolver,
        grants,
        hooks,
        // The default 4-arg wrapper keeps strict root containment — escape-root
        // approval is opt-in, wired only by the full head (`main.rs`).
        None,
        mode_table,
        // These convenience wrappers' (~30, test-only) callers never wire up a
        // plans-folder watch either, so a private, unshared registry is fine —
        // matches their historical no-external-sharing behavior for `jobs`/
        // `retained` above.
        Arc::new(PlanFileRegistry::new()),
        // No per-user MCP scopes (#684) — single-user, like skutter itself.
        None,
        // No advertising-resolution inputs (ADR-0196 Phase P1): these
        // wrappers' (~30, test-only) callers hand no catalog/config pair, so
        // every session resolves `tool_search` — the map still folds,
        // nothing dispatches on it yet.
        None,
        // No discovery surface (ADR-0196 §4): these wrappers' (~30, test-only)
        // callers build no `tool_spec_resolver` of their own either, so a
        // private, unshared `AdvertisingState` and an empty MCP roster (
        // `explore` simply has nothing MCP to report) are exactly right.
        None,
    )
}

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
/// Escape-root policy for the executor (ADR-0109): the canonical project `root`
/// against which an out-of-root `read`/`edit`/`write` path or `bash`/`call`
/// `workdir` is detected, plus the shared [`ExtraRootStore`] approvals are
/// recorded into and the host tools read. `None` (the wrappers below, all tests)
/// keeps strict containment — an out-of-root path is a hard error, never a
/// prompt.
///
/// `plan_files` (#513) is the shared [`PlanFileRegistry`] this executor's
/// `propose_plan` dispatch arm and its own `FileChange` listener (spawned
/// inside, below) both read/write. Taken as a param (#627) rather than
/// constructed internally so a caller wiring up the dedicated plans-folder
/// watch ([`crate::plan_watch::spawn_plans_watcher`]) can hand it the exact
/// same instance — the watch's out-of-band notice and this executor's
/// staleness guard must agree on what the agent last knew.
#[derive(Clone)]
pub struct EscapeRoot {
    pub root: std::path::PathBuf,
    pub store: Arc<crate::extra_roots::ExtraRootStore>,
}

impl EscapeRoot {
    /// The absolute out-of-root path a call to `tool` with `input` would touch,
    /// or `None` when it stays contained (or the tool has no path argument).
    /// `pub(crate)`: also called from [`crate::script`], whose bindings route
    /// through this same gate (#446).
    pub(crate) fn escaping(&self, tool: &str, input: &str) -> Option<std::path::PathBuf> {
        let rel = crate::permission::escape_root_target(tool, input)?;
        crate::host::escaping_path(&self.root, &rel)
    }
}

/// `explore`/`describe`'s shared inputs (#560, ADR-0196 §4), bundled into one
/// struct — see [`spawn_tool_executor_with_policy`]'s `discovery` param.
/// `Default` gives every field's own empty/private state: an advertising
/// state no external resolver shares, an empty MCP roster, and a private
/// loop-breaker tracker.
#[derive(Default)]
pub struct DiscoverySurface {
    pub advertising: SharedAdvertisingState,
    pub mcp_avail: Arc<AvailableMcp>,
    pub mcp_active: ActiveServers,
    /// The pre-dispatch argument-validation loop-breaker guard (#560,
    /// ADR-0196 §6) — bundled here rather than as a fourth top-level param
    /// since it's session-keyed state alongside `advertising`, read/written
    /// by the same `dispatch`/`run_and_reply` call sites.
    pub validation: Arc<arg_validate::LoopBreaker>,
    /// The shared endpoint-pool `HttpClient` a dispatch-time lazy MCP
    /// re-enable rides (ADR-0201, `mcp::available::try_lazy_reenable`) —
    /// `None` (the convenience wrappers, every test-only caller) degrades a
    /// would-be re-enable to a clean tool error rather than a panic; those
    /// callers' `mcp_avail` is also the empty default, so the path is never
    /// actually exercised.
    pub http: Option<entanglement_core::HttpClient>,
}

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
    let mut sub = holly.subscribe();
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
        // Active agent per session. Folded from lifecycle events, but the fold
        // is a *lossy* broadcast — so it is authoritatively self-healed on every
        // `ToolExec` from the agent name that event carries (#156). See the
        // `ToolExec` arm below. Shared (`Arc<Mutex<..>>`, a param) with the
        // default `ModeResolver` (#311) so it reads the same folded view; this
        // loop is the sole writer, so the brief locks never contend.
        //
        // Per-session *in-flight* request_id dedupe (#274, ADR-0071): the set of
        // `ToolExec` request ids this executor has dispatched but not yet seen
        // resolved. Core arms a re-offer timer while a turn is parked and re-emits
        // the pending batch after a stretch of silence (its recovery for an offer
        // dropped under broadcast lag), so the *same* `ToolExec` can arrive twice
        // — once as the original, once as a re-offer while the first is still
        // running. Running it twice would double-execute a `bash`/`edit`/spawn, so
        // an id still in flight is skipped. An id is dropped again on the
        // `ToolOutput` core emits when the call resolves (its result was folded),
        // so a *later* round that legitimately reuses the same id — core matches
        // by id only within the current round's pending set — is not wrongly
        // skipped. This loop is single-threaded (it routes before spawning the
        // detached handler, and consumes `ToolExec`/`ToolOutput` in broadcast
        // order), so the check is race-free without a lock. Cleared per session on
        // `SessionEnded`.
        let mut in_flight: HashMap<SessionId, HashSet<String>> = HashMap::new();
        // Per-session live tool overlay (#539, ADR-0149), folded from
        // `ToolOverlayChanged` — the dispatch-side mirror of core's
        // `Session::tool_overlay`. Consulted for the Ask/Allow grade override
        // the generic route applies in `dispatch` (an enable entry can
        // override even a mode `deny` for that session, ADR-0207 §8) and
        // dropped wholesale on a mode change (the overlay is mode-scoped).
        // Loop-owned: the per-call overlay entry is resolved before the
        // detached task is spawned, so no sharing is needed. Cleared on
        // `SessionEnded`/`SessionHibernated`.
        let mut overlays: HashMap<SessionId, Vec<entanglement_core::ToolOverlayEntry>> =
            HashMap::new();
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
        // Per-session plan-file staleness tracking (#513): the registry passed
        // in above, kept fresh by `propose_plan` itself (below), passively by
        // the `FileChange` listener spawned right after this loop starts, and
        // — out of band — by `plan_watch::spawn_plans_watcher` if the caller
        // wired one up against this same instance (#627).
        {
            let mut file_changes = holly.subscribe();
            let plan_files = plan_files.clone();
            let plan_root = plan_root.clone();
            background.spawn(async move {
                loop {
                    match file_changes.recv().await {
                        Ok(OutEvent::FileChange {
                            session,
                            path,
                            hash,
                            ..
                        }) => {
                            let rel =
                                crate::permission_path::rooted_arg(&plan_root, "write", &path);
                            plan_files.note_file_change(&session, &rel, &hash);
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {
                            // Best-effort: a missed `FileChange` just means the
                            // registry's next staleness check treats an
                            // in-session edit as if it were external — fails
                            // closed (asks the agent to re-read), never open.
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            });
        }
        // Bounds the spawn tree (#76): tracks parent links from lifecycle events
        // and per-root running sub-agent counts. Lives in this single-threaded loop, so the
        // spawn decision below is race-free.
        let mut spawn_guard = crate::subagent::SpawnGuard::new();
        // Per-session tool advertising (ADR-0196 §2-3): pinned at session
        // start from the session's initial model, kept across `SetModel`
        // (logged when the new model's catalog preference differs), released
        // on end/hibernate. `advertising` (the mode map plus the discovered-
        // tool set `describe` writes into) is caller-constructed and shared
        // — Phase P1's loop-local-only map is gone; the `tool_spec_resolver`/
        // `system_prompt_resolver` closures in `main.rs` read the same `Arc`.
        // `advertising_inputs == None` (the convenience wrappers, tests)
        // keeps the fold running so the shape is identical, resolving
        // `tool_search` throughout.
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
        {
            // The single inbound router (#156): the *sole* consumer of the inbound
            // fan-out for decisions. It watches `Stop` (cancel in-flight tools +
            // unwind parked approvals, #167), fires the `user_prompt_submit` hooks
            // (#199) off each `Prompt`, resolves every `Approve`/`Reject`/
            // `Answer`/`RetractQuestion`/`ReplaceQuestion` (#515) to its parked
            // waiter, and answers `InMsg::ListQuestions` (#515)/`InMsg::ListOperations`
            // (#607) directly from `open_questions`/the job+agent registries — read-only
            // snapshot queries, not decisions, so they don't go through `pending`.
            // One light map-lookup-per-frame loop drains far faster than a park
            // loop, so it does not lag the way the per-task subscriptions it
            // replaced did.
            let cancels = cancels.clone();
            let hooks = hooks.clone();
            let pending = pending.clone();
            let open_questions = open_questions.clone();
            let op_jobs = jobs.clone();
            let op_agents = registry.clone();
            let op_scripts = scripts.clone();
            let emitter = holly.clone();
            let mut inbound = inbound;
            background.spawn(async move {
                loop {
                    match inbound.recv().await {
                        Ok(InMsg::Stop { session }) => {
                            cancels.cancel_session(&session);
                            pending.stop_session(&session);
                        }
                        Ok(InMsg::Prompt { session, content })
                            if !hooks.user_prompt_submit.is_empty() =>
                        {
                            // Detach so a slow hook can't stall the router.
                            let hooks = hooks.clone();
                            tokio::spawn(async move {
                                let text = entanglement_core::content_text(&content);
                                hooks.run_user_prompt_submit(&session, &text).await;
                            });
                        }
                        Ok(InMsg::ListQuestions {
                            correlation_id,
                            session,
                        }) => {
                            let questions = open_questions.snapshot(session.as_ref());
                            emitter.emit_question_list(correlation_id, questions);
                        }
                        Ok(InMsg::ListOperations {
                            correlation_id,
                            session,
                        }) => {
                            let operations = crate::operations::list_operations(
                                &op_jobs,
                                &op_agents,
                                &op_scripts,
                                session.as_ref(),
                            );
                            emitter.emit_operation_list(correlation_id, operations);
                        }
                        Ok(other) => {
                            if let Some((s, rid, decision)) = seam::Decision::from_inmsg(other) {
                                pending.resolve(&s, &rid, decision);
                            }
                        }
                        // A lagging router would strand a decision; warn loudly.
                        // In practice this loop can't fall behind the inbound fill
                        // rate — this is not the #156 failure mode it fixes.
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(
                                skipped = n,
                                "decision router lagged; some inbound frames dropped"
                            );
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            });
        }
        // `max_turns`/`max_duration` enforcement (ADR-0207 §11, stage 5c):
        // its own subscriber, parked in the same `JoinSet` as every other
        // background task here so it's aborted alongside them rather than
        // leaking a `Holly` clone past shutdown.
        {
            let holly = holly.clone();
            let mode_table = mode_table.clone();
            background.spawn(crate::run_budget::watch(holly, budget_sub, mode_table));
        }
        // The ladder's long-lived shared state (issue #451), cloned once
        // here rather than built by moving the loop-locals above: every
        // field is `Arc`-cheap to clone, and cloning (instead of moving)
        // leaves each original binding intact for the lifecycle-folding
        // arms below, which keep reading several of them directly
        // (`agents`, `active`, `perm_modes`, …).
        let ctx = LadderCtx {
            holly: holly.clone(),
            registry: registry.clone(),
            retained: retained.clone(),
            jobs: jobs.clone(),
            scripts: scripts.clone(),
            pending: pending.clone(),
            open_questions: open_questions.clone(),
            resolver: resolver.clone(),
            agents: agents.clone(),
            perm_modes: perm_modes.clone(),
            mode_table: mode_table.clone(),
            plan_files: plan_files.clone(),
            plan_root: plan_root.clone(),
            cancels: cancels.clone(),
            tools: tools.clone(),
            skills: skills.clone(),
            mcp_avail: mcp_avail.clone(),
            mcp_active: mcp_active.clone(),
            mcp_scopes: mcp_scopes.clone(),
            advertising: advertising.clone(),
            escape_root: escape_root.clone(),
            base: base.clone(),
            grants: grants.clone(),
            mcp_http: mcp_http.clone(),
            active_skill: active_skill.clone(),
            hooks: hooks.clone(),
            validation: validation.clone(),
            denials: denials.clone(),
            // #560 P12, ADR-0207 §12: read off the same `advertising_inputs`
            // every session-start resolution already consults.
            catalog: advertising_inputs
                .as_ref()
                .and_then(|i| i.catalog().cloned()),
        };
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted {
                    session,
                    parent,
                    agent,
                    ..
                }) => {
                    spawn_guard.record_start(session.clone(), parent.clone());
                    // Tool advertising is NOT pinned here: this broadcast
                    // races core's first round, so the tool-spec resolver
                    // pins at first resolution (`AdvertisingState::
                    // ensure_pinned`, ADR-0204).
                    // A head-driven resume (ADR-0112) re-emits `SessionStarted`
                    // for a previously-hibernated child (#609, ADR-0162 §4) — a
                    // no-op for any other session, since a fresh registration is
                    // already `Live` and an untracked id has no entry to update.
                    registry.mark_live(&session);
                    let started_agent = agents
                        .read()
                        .expect("agent-catalog lock poisoned")
                        .get(&agent)
                        .cloned();
                    if let Some(p) = started_agent.clone() {
                        active
                            .lock()
                            .expect("active-agent mutex poisoned")
                            .insert(session.clone(), p);
                    }
                }
                Ok(OutEvent::AgentChanged { session, agent }) => {
                    if let Some(p) = agents
                        .read()
                        .expect("agent-catalog lock poisoned")
                        .get(&agent)
                        .cloned()
                    {
                        active
                            .lock()
                            .expect("active-agent mutex poisoned")
                            .insert(session, p);
                    }
                }
                // The session's permission mode changed (ADR-0207 stage 4):
                // fold the same way `active` folds `AgentChanged`, so
                // `ModeResolver` reads the current mode. Fires once at
                // session start (mirroring `AgentChanged`) and again on every
                // `InMsg::SetMode`. The session's own tool overlay is
                // mode-scoped (ADR-0207 §8, "the overlay becomes mode-scoped")
                // — it is dropped on any *actual* mode change, since an entry
                // enabled under one mode carries no meaning in another; a
                // duplicate `ModeChanged` for the same value (replay, a
                // no-op `SetMode`) leaves it untouched.
                Ok(OutEvent::ModeChanged { session, mode }) => {
                    let previous = perm_modes
                        .lock()
                        .expect("permission-mode mutex poisoned")
                        .insert(session.clone(), mode.clone());
                    if previous.as_deref() != Some(mode.as_str()) {
                        overlays.remove(&session);
                    }
                }
                // The session's model changed (`SetModel` / an agent pin
                // re-bind, #218/#323). Tool advertising is *not* re-resolved
                // (ADR-0196 §2): a pinned session keeps its mode — switching
                // mid-session would bust the prompt cache the mode protects —
                // and this only logs when the new model prefers another one.
                Ok(OutEvent::ModelChanged {
                    session,
                    provider,
                    model,
                    ..
                }) => {
                    if let Some(inputs) = advertising_inputs.as_ref() {
                        let modes = advertising
                            .modes
                            .lock()
                            .expect("tool-advertising mode mutex poisoned");
                        inputs.note_model_changed(&modes, &session, &provider, &model);
                    }
                }
                // A hibernated session (#318) tore down just like an ended one, so
                // its executor-side bookkeeping is equally moot — release it. Its
                // persisted "always" grants survive; a resume rebuilds the rest.
                // The session's live tool overlay changed (#539, ADR-0149):
                // mirror core's full-replacement semantics — an empty list
                // clears the entry entirely.
                Ok(OutEvent::ToolOverlayChanged { session, entries }) => {
                    // An enable never touches the advertised array (ADR-0204):
                    // the tool becomes explore-visible and dispatchable, and
                    // is appended only when its schema is delivered.
                    if entries.is_empty() {
                        overlays.remove(&session);
                    } else {
                        overlays.insert(session, entries);
                    }
                }
                Ok(ev @ OutEvent::SessionEnded { .. })
                | Ok(ev @ OutEvent::SessionHibernated { .. }) => {
                    let session = ev
                        .session()
                        .cloned()
                        .expect("SessionEnded/SessionHibernated always carry a session");
                    // Fold the lifecycle transition into the agent registry
                    // (#609, ADR-0162 §4) so `agent_send` can refuse a closed
                    // or hibernated child instead of letting the supervisor's
                    // lazy-`Prompt` path silently respawn it blank. A no-op
                    // for a session this registry never tracked.
                    if matches!(ev, OutEvent::SessionHibernated { .. }) {
                        registry.mark_hibernated(&session);
                    } else {
                        registry.mark_closed(&session);
                    }
                    // Drop the closed session's in-memory grants (#174); persisted
                    // "always" grants survive.
                    grants.forget_session(&session);
                    // The live tool overlay dies with the session (#539) — a
                    // resume replays core's `ToolOverlayChanged` records, which
                    // re-emit on the broadcast and re-fold here.
                    overlays.remove(&session);
                    // Its in-flight tool bookkeeping is moot once the session ends.
                    cancels.forget_session(&session);
                    // Drop the re-offer dedupe set (#274): its request ids can
                    // never recur once the session is gone.
                    in_flight.remove(&session);
                    // The active-skill posture tracking is moot once the
                    // session is gone too (#400) — no `Done` will follow to
                    // clear it otherwise.
                    active_skill
                        .lock()
                        .expect("active-skill mutex poisoned")
                        .remove(&session);
                    // No sandbox cache to drop any more (ADR-0207 §6, stage
                    // 5b): confinement reads `perm_modes` directly, and that
                    // map's own entry is left in place like `active`'s — a
                    // resume re-emits `ModeChanged` before anything reads it.
                    // The plan-file staleness binding (#513) is moot too.
                    plan_files.forget_session(&session);
                    // And its pinned tool advertising, discovered set, `Full`
                    // snapshot and `<env>` date (ADR-0196 §2-3, ADR-0202 §5) —
                    // a resume re-pins at its first round.
                    advertising.forget(&session);
                    // The loop-breaker's last-call tracker (#560, ADR-0196
                    // §6) is equally session-scoped — nothing to break a
                    // loop against once the session is gone.
                    validation.forget(&session);
                    // The repeat-denial tracker (ADR-0207 §11) is per-turn
                    // scoped, so it's moot once the session itself is gone.
                    denials.clear(&session);
                }
                // A skill's "active" posture scopes one model turn (#400,
                // ADR-0106; posture-only since ADR-0194): clear it here so a
                // later turn can `load_skill` a different one (or none)
                // cleanly, and tell any listening head via
                // `OutEvent::SkillActive { skill_id: None, .. }`. The
                // repeat-denial tracker (ADR-0207 §11) shares this same
                // per-turn scope.
                Ok(OutEvent::Done { session, .. }) => {
                    clear_active_skill(&holly, &active_skill, &session);
                    denials.clear(&session);
                }
                // A `Stop` that lands while a batch is parked unwinds with no
                // `ToolResult`/`ToolOutput` for its still-running calls (#448):
                // core clears the parked turn state and emits a terminal
                // `Status` without ever resolving them, so the #274 dedupe set
                // above would otherwise leak their request ids for the rest of
                // the session's life. Core only ever sends `Idle` on session
                // start (before any `ToolExec`, so the set is already empty) or
                // — since ADR-0139 — `Done` on a `Stop` (never mid-turn), so
                // dropping the session's whole set is exactly "no call is in
                // flight any more", not an approximation. `Idle` is kept for
                // the genuine start case; both states fold to the same cleanup.
                Ok(OutEvent::Status {
                    session,
                    state: AgentState::Idle | AgentState::Done,
                    ..
                }) => {
                    in_flight.remove(&session);
                }
                // A resolved call (#274): core folded its result and emitted this
                // `ToolOutput`, so the id is no longer in flight — drop it from the
                // dedupe set. This frees the id for a later round to reuse (core
                // matches by id only within a round's pending set) while keeping an
                // *unresolved* in-flight call guarded against a double-run re-offer.
                Ok(OutEvent::ToolOutput {
                    session,
                    request_id,
                    ..
                }) => {
                    if let Some(set) = in_flight.get_mut(&session) {
                        set.remove(&request_id);
                    }
                }
                // The parked `ToolExec` seq is deliberately ignored (#157): every
                // event the runtime authors around this call (an approval
                // `ToolRequest`/`UserQuestion`, a `Plan`/`TaskList` snapshot, a
                // `FileChange`) mints a fresh per-session seq via
                // `Holly::emit_for_session`, so `(session, seq)` stays unique.
                Ok(OutEvent::ToolExec {
                    session,
                    request_id,
                    tool,
                    input,
                    agent,
                    envelope,
                    ..
                }) => {
                    // Idempotence for core's re-offer timer (#274, ADR-0071):
                    // skip a request id whose call is still in flight. Core
                    // re-offers a parked batch after silence to recover an offer
                    // dropped under broadcast lag; a re-offer of a call this
                    // executor is already running must not run a second time. The
                    // first offer records the id; a re-offer while it is unresolved
                    // is a no-op. The id is dropped on the resolving `ToolOutput`
                    // below, so a later round reusing it still dispatches.
                    if !in_flight
                        .entry(session.clone())
                        .or_default()
                        .insert(request_id.clone())
                    {
                        tracing::debug!(
                            %request_id,
                            "skipping re-offered ToolExec (still in flight)"
                        );
                        continue;
                    }
                    // Authoritative self-heal (#156): the emitting session's
                    // active agent rides on the `ToolExec` itself, so resolve it
                    // from the registry and overwrite the folded entry *before*
                    // any permission decision (spawn gating and the `rhai`
                    // binding policy still read `active`; sandboxing reads
                    // `perm_modes` directly, ADR-0207 §6). The lifecycle
                    // fold above is a lossy broadcast — under burst a dropped
                    // `SessionStarted`/`AgentChanged` would leave a restricted
                    // session unseen and (pre-#156) fail *open*. This makes the
                    // leaf's gate authoritative regardless of that drop; the
                    // fail-closed `ModeResolver` default (an unseen session's
                    // mode) covers only the residual unknown case.
                    if let Some(p) = agents
                        .read()
                        .expect("agent-catalog lock poisoned")
                        .get(&agent)
                        .cloned()
                    {
                        active
                            .lock()
                            .expect("active-agent mutex poisoned")
                            .insert(session.clone(), p);
                    }
                    // The tool mask is retired (ADR-0207 §8, "the mask
                    // machinery is deleted"): every tool advertised is
                    // dispatchable, graded by the session's permission mode
                    // instead of withheld by an agent allowlist. The
                    // routes below are a `match` (mutually exclusive), so
                    // adding one is a compiler-checked exhaustiveness change,
                    // not an ordering hazard. Each handler runs on its own
                    // task; the loop only routes.
                    let route = Intercept::classify(&tool);
                    tracing::trace!(
                        %tool,
                        ?route,
                        bypasses_permission = route.bypasses_permission(),
                        "routing tool exec"
                    );
                    ladder::route_tool_exec(
                        &ctx,
                        &mut spawn_guard,
                        &overlays,
                        route,
                        envelope,
                        session,
                        request_id,
                        tool,
                        input,
                    )
                    .await;
                }
                Ok(_) => {}
                // A lagging executor drops broadcast events; the affected turn
                // stays parked, but that's preferable to executing stale calls.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "tool executor lagged; some ToolExec dropped");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// Clear `session`'s skill-active posture (#400, ADR-0106) — the turn's
/// `Done`, the natural end of a skill's scope. A no-op (no wire event) when
/// no skill was active, matching [`activate_skill`]'s "only tell a head about
/// a real change" shape.
fn clear_active_skill(
    holly: &Holly,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    session: &SessionId,
) {
    if active_skill
        .lock()
        .expect("active-skill mutex poisoned")
        .remove(session)
    {
        holly.emit_for_session(session, |seq| OutEvent::SkillActive {
            session: session.clone(),
            seq,
            skill_id: None,
            allowed_tools: None,
        });
    }
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
