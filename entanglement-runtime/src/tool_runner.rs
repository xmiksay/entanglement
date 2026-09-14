//! Runtime tool executor. Owns everything about a tool call that is *not* the
//! engine's business: the `Allow | Ask | Deny` permission decision (#59), the
//! approval UX round-trip, and the actual execution against the host-tool
//! [`ToolRegistry`] (#58, ADR-0006/0010).
//!
//! Core emits [`OutEvent::ToolExec`] for **every** host tool and parks on
//! [`InMsg::ToolResult`]; it no longer consults `PermissionProfile`. This task:
//!
//! 1. tracks each session's active [`AgentProfile`] — folded from `SessionStarted`
//!    / `AgentChanged` (ADR-0020) but **self-healed** on every `ToolExec` from the
//!    profile name the event carries (#156), resolved against the
//!    [`ProfileRegistry`] handed at startup. That fold is a *lossy* broadcast, so
//!    under burst a dropped lifecycle event would otherwise leave a restricted
//!    session unseen; the self-heal makes the gate authoritative, and the
//!    `permission_for`/`tool_masked` defaults fail *closed* (`Deny`/masked) for the
//!    residual unknown case rather than the pre-#156 allow-all fallback that
//!    inverted the security posture under overload;
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
#[cfg(feature = "rhai")]
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{
    AgentProfile, AgentState, ApprovalScope, Holly, IdKind, InMsg, OutEvent, Permission,
    PermissionProfile, ProfileRegistry, SessionId, ToolCall,
};

use crate::tools::{SharedRegistry, ToolExecution, ToolRegistry};
use tokio::sync::broadcast::error::RecvError;

use crate::arg_validate;
use crate::cancel::{CancelAllOnDrop, CancelRegistry, TaskCanceller};
use crate::discover;
use crate::hooks::Hooks;
use crate::mcp::{ActiveServers, AvailableMcp};
#[cfg(feature = "rhai")]
use crate::permission::effective_permission;
use crate::permission::{
    ancestor_chain, clamp_to_base, min_permission, spawn_refusal, tool_mask_source,
};
use crate::permission_path::grading_arg;
use crate::plan_files::PlanFileRegistry;
use crate::policy::{DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver};
use crate::seam;
use crate::skills::load_skill::parse_skill_id;
use crate::skills::SkillRegistry;
use crate::tool_advertising::{self, SharedAdvertisingState};
#[cfg(feature = "rhai")]
use crate::tool_names::RHAI_TOOL;
use crate::tool_names::{
    is_non_maskable, AGENT_SEND_TOOL, AGENT_TOOL, ASK_USER_TOOL, DESCRIBE_TOOL, EXPLORE_TOOL,
    LOAD_SKILL_TOOL, POLL_TOOL, PROPOSE_PLAN_TOOL, RESPONSES_TOOL_SEARCH_TOOL,
};

/// Upgrade a resolved `Ask` to `Allow` when `(session, tool, arg)` is already
/// granted (#174): a session-scoped or persisted "always allow" grant lets an
/// *identical* later call skip the prompt. Only `Ask` is widened — a `Deny` (a
/// hard policy floor) and an outright `Allow` pass through untouched.
fn apply_grant(
    grants: &dyn GrantStore,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
    perm: Permission,
) -> Permission {
    if perm == Permission::Ask && grants.is_granted(session, tool, arg) {
        Permission::Allow
    } else {
        perm
    }
}

/// Least-privileged resolver grade across a call's ancestor chain — the sub-agent
/// privilege ceiling (ADR-0024) applied *on top of* whatever the pluggable
/// [`PermissionResolver`] returns, so a tenant rule can never widen a child
/// beyond its parent. For the default [`ProfileResolver`] this reproduces
/// `effective_permission` + `clamp_to_base` (the clamp is monotonic, so
/// min-of-clamped equals clamp-of-min). An empty chain is impossible — the leaf
/// session is always present — but defaults to `Deny` if one ever arrives.
async fn resolve_effective(
    resolver: &dyn PermissionResolver,
    chain: &[SessionId],
    tool: &str,
    input: &str,
) -> Permission {
    let mut perm = Permission::Allow;
    let mut any = false;
    for session in chain {
        perm = min_permission(perm, resolver.resolve(session, tool, input).await);
        any = true;
    }
    if any {
        perm
    } else {
        Permission::Deny
    }
}

/// How the executor routes a `ToolExec` once the tool mask (#116) has cleared.
///
/// Classification is a **pure function of the tool name** ([`Intercept::classify`]),
/// which makes the ladder's one load-bearing invariant — the mask precedes every
/// route (#203) — structural rather than comment-enforced: the loop checks
/// [`tool_masked`] before it ever calls `classify`, and the routes are a `match`
/// (mutually exclusive) instead of a fall-through chain of `if tool == X { … }`
/// branches, so a newly added route can no longer be silently mis-ordered ahead
/// of the mask. Adding a tool means adding a variant here and its `match` arm in
/// the dispatch loop — both checked by the compiler's exhaustiveness rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intercept {
    /// `agent`: session orchestration only (touches no host resource), gated by
    /// the per-profile spawn control, not per-tool approval (#60/#119/#120;
    /// #606, ADR-0161 §1). Blocks for the answer by default; the parsed
    /// `background` flag picks the non-blocking launch instead — one guard
    /// path, two return shapes.
    Spawn,
    /// `agent_send`: sends a follow-up prompt to a sub-agent already launched
    /// with `agent` — steer a running child, follow up a finished one, or
    /// re-engage a `propose_plan` sponsored build (#609, ADR-0162). Session
    /// orchestration only, like `Spawn` — gated by
    /// [`crate::agent_registry::AgentRegistry::begin_send`]'s ownership +
    /// lifecycle check instead of per-tool approval.
    AgentSend,
    /// `poll`: joins a background `bash` job or a launched sub-agent (#605,
    /// ADR-0161 §1-4, replacing `bash_output`/`agent_poll`) — it reads
    /// accumulated job/spawn state, starting no session and touching no host.
    Poll,
    /// `ask_user`: a runtime-owned prompt tool (#90, ADR-0027) that surfaces a
    /// question to the head instead of running against the registry.
    AskUser,
    /// `propose_plan`: the plan agent's finalize step (#141, ADR-0042),
    /// force-parked on the `Ask` path since user approval *is* its semantics.
    ProposePlan,
    /// `explore`/`describe` (#560, ADR-0196 §4) plus the reserved
    /// `responses_tool_search` call name (P7, ADR-0196 §3 — a streamed
    /// client-executed `tool_search_call` from the OpenAI Responses wire,
    /// never a name the model chose from an advertised schema): the
    /// always-on, non-maskable discovery trio — read-only catalog
    /// introspection, starting nothing and touching no host resource. Exempt
    /// from the #116 mask entirely (see the `is_non_maskable` short-circuit
    /// ahead of classification, not this route), and from the
    /// `Allow`/`Ask`/`Deny` ladder like every other runtime-owned
    /// orchestration tool.
    Discover,
    /// `rhai`: a sandboxed script tool (#122, ADR-0046) that resolves its own
    /// permission live against the loop's profile snapshot inside the script task.
    /// Behind the `rhai` feature (#502, ADR-0135) — a lean build without it
    /// never registers the tool, so a call named `rhai` falls through to the
    /// generic `Permission` route below and is refused there as unknown.
    #[cfg(feature = "rhai")]
    Rhai,
    /// Every other host tool: the generic `Allow | Ask | Deny` dispatch.
    Permission,
}

impl Intercept {
    /// Route an (already-unmasked) tool by name.
    fn classify(tool: &str) -> Self {
        match tool {
            AGENT_TOOL => Self::Spawn,
            AGENT_SEND_TOOL => Self::AgentSend,
            POLL_TOOL => Self::Poll,
            ASK_USER_TOOL => Self::AskUser,
            PROPOSE_PLAN_TOOL => Self::ProposePlan,
            EXPLORE_TOOL | DESCRIBE_TOOL | RESPONSES_TOOL_SEARCH_TOOL => Self::Discover,
            #[cfg(feature = "rhai")]
            RHAI_TOOL => Self::Rhai,
            _ => Self::Permission,
        }
    }

    /// Whether this route skips the per-tool `Allow | Ask | Deny` decision. The
    /// spawn/poll/prompt/plan/discover routes touch no host resource, so
    /// permission does not apply; `Rhai` resolves permission itself inside the
    /// script task; the generic `Permission` route *is* the permission
    /// decision.
    fn bypasses_permission(self) -> bool {
        matches!(
            self,
            Self::Spawn
                | Self::AgentSend
                | Self::Poll
                | Self::AskUser
                | Self::ProposePlan
                | Self::Discover
        )
    }
}

/// Spawn the per-engine tool executor. Subscribes synchronously (so no
/// `ToolExec` emitted before the task is scheduled is missed) and runs until the
/// engine's outbox closes. `profiles` is the runtime's copy of the engine's
/// [`ProfileRegistry`] — the permission *shape* stays a core type; the runtime
/// only reads it (ADR-0003). `base` is the user config's global permission
/// ceiling (#172): every resolved grade is clamped least-privilege against it.
pub fn spawn_tool_executor(
    holly: &Holly,
    tools: ToolRegistry,
    profiles: ProfileRegistry,
    base: PermissionProfile,
) -> tokio::task::JoinHandle<()> {
    spawn_tool_executor_with_hooks(holly, tools, profiles, base, Hooks::default())
}

/// Wrap a caller's [`SkillRegistry`] for [`spawn_tool_executor_with_policy`]'s
/// `skills` parameter, mirroring [`wrap_profiles`]. The convenience wrappers
/// below plug in an empty registry — no `load_skill` mask ever activates for
/// their (~30, test-only) callers, matching their historical no-skill-mask
/// behavior byte-for-byte.
fn wrap_skills(skills: SkillRegistry) -> Arc<RwLock<Arc<SkillRegistry>>> {
    Arc::new(RwLock::new(Arc::new(skills)))
}

/// Wrap a caller's owned [`ProfileRegistry`] for [`spawn_tool_executor_with_policy`],
/// which reads it through an `Arc<RwLock<..>>` so a live definitions watcher
/// (#329) can swap it for a fresher one without restarting the executor. The
/// convenience wrappers here keep their historical owned-registry signature for
/// existing callers (and tests) that need no live reload.
fn wrap_profiles(profiles: ProfileRegistry) -> Arc<RwLock<ProfileRegistry>> {
    Arc::new(RwLock::new(profiles))
}

/// Like [`spawn_tool_executor`] but with user-configured lifecycle hooks (#199,
/// ADR-0066): `pre_tool_use` can veto a generic tool dispatch, `post_tool_use`
/// runs as a side-effect after it, and `user_prompt_submit` fires on every
/// inbound `Prompt`. The no-hook wrapper keeps the historical 4-arg signature for
/// callers (and tests) that need no hooks.
pub fn spawn_tool_executor_with_hooks(
    holly: &Holly,
    tools: ToolRegistry,
    profiles: ProfileRegistry,
    base: PermissionProfile,
    hooks: Hooks,
) -> tokio::task::JoinHandle<()> {
    // The default single-user policy (#311): the executor folds lifecycle events
    // into `active`, and the default `ProfileResolver` reads that same map so its
    // grade stays byte-identical with the pre-seam `effective_permission` path.
    // "Always allow" grants persist to the managed file.
    let active = Arc::new(Mutex::new(HashMap::new()));
    // No escape-root policy wired here (root: None) — the strict-containment
    // 4-arg wrapper keeps the pre-#485 verbatim arg match (ADR-0125).
    let resolver: Arc<dyn PermissionResolver> =
        Arc::new(ProfileResolver::new(active.clone(), base.clone(), None));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    spawn_tool_executor_with_policy(
        holly,
        tools.shared(),
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
        wrap_profiles(profiles),
        wrap_skills(SkillRegistry::default()),
        base,
        active,
        resolver,
        grants,
        hooks,
        // The default 4-arg wrapper keeps strict root containment — escape-root
        // approval is opt-in, wired only by the full head (`main.rs`).
        None,
        // No per-profile sandboxing wired here (#479) — every `bash`/`call` in
        // this wrapper's callers runs unsandboxed, byte-identical to pre-#479.
        crate::policy::SandboxConfig::none(),
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
/// the shared per-session profile map the executor folds lifecycle events into —
/// still driving tool masking (#116) and spawn gating (#119), which stay in the
/// ladder on top of the resolver — and which the default [`ProfileResolver`]
/// reads. The two default wrappers above plug in [`ProfileResolver`] +
/// [`DefaultGrantStore`] for the CLI, byte-identical to the pre-seam behavior.
///
/// `profiles` is behind an `Arc<RwLock<..>>` (#329, not a plain owned
/// [`ProfileRegistry`]) so a runtime definitions watcher can swap in a
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
/// (never held across a tool's `.await`), mirroring the `profiles` pattern
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
/// jobs `bash` actually spawned; unlike `tools`/`skills`/`profiles` this isn't
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
    profiles: Arc<RwLock<ProfileRegistry>>,
    skills: Arc<RwLock<Arc<SkillRegistry>>>,
    base: PermissionProfile,
    active: Arc<Mutex<HashMap<SessionId, AgentProfile>>>,
    resolver: Arc<dyn PermissionResolver>,
    grants: Arc<dyn GrantStore>,
    hooks: Hooks,
    escape_root: Option<EscapeRoot>,
    // Per-profile bubblewrap confinement for `bash`/`call` (#479, ADR-0104
    // amendment): `own`/`floor` are folded from the same lifecycle events as
    // `active` below, and read by the caller's `SandboxConfig::resolver()`.
    sandbox: crate::policy::SandboxConfig,
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
    } = discovery.unwrap_or_default();
    let hooks = Arc::new(hooks);
    let mut sub = holly.subscribe();
    // Subscribe to the inbound fan-out *synchronously*, before this function
    // returns, so a `Prompt`/`Stop` the caller sends right after spawning can't
    // race ahead of the watcher's subscription (the `user_prompt_submit` hook,
    // #199, depends on catching that first prompt).
    let inbound = holly.subscribe_inbound();
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
        // Active profile per session. Folded from lifecycle events, but the fold
        // is a *lossy* broadcast — so it is authoritatively self-healed on every
        // `ToolExec` from the profile name that event carries (#156). See the
        // `ToolExec` arm below. Shared (`Arc<Mutex<..>>`, a param) with the
        // default `ProfileResolver` (#311) so it reads the same folded view; this
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
        // `Session::tool_overlay`. Consulted by `tool_masked` (a matching entry
        // makes the tool exist regardless of the profile mask, per link) and
        // for the Ask/Allow grade override the generic route applies in
        // `dispatch`. Loop-owned: the per-call overlay entry is resolved before
        // the detached task is spawned, so no sharing is needed. Cleared on
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
        // and per-root spawn budgets. Lives in this single-threaded loop, so the
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
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted {
                    session,
                    parent,
                    profile,
                    model,
                    ..
                }) => {
                    spawn_guard.record_start(session.clone(), parent.clone());
                    // Tool advertising pinned at start (ADR-0196 §2): resolved
                    // from *this session's* initial model, so concurrent
                    // sessions can differ (a pinned cheap-model `explore`
                    // child vs its parent). The start pairing: `model` here
                    // is the profile's bare model field; a pin-driven
                    // rebind's `ModelChanged` (provider+model) follows
                    // immediately for a pinned profile and pins the precise
                    // pair via the `ModelChanged` arm's first-observation
                    // rule below. `pin` is idempotent-safe by design — the
                    // start pair is the authority, a later re-observed start
                    // (resume) re-resolves the same value.
                    if let Some(inputs) = advertising_inputs.as_ref() {
                        let mut modes = advertising
                            .modes
                            .lock()
                            .expect("tool-advertising mode mutex poisoned");
                        inputs.pin_session_start(
                            &mut modes,
                            &session,
                            // The startup default's provider name is not
                            // announced (`Session::provider` starts `None`,
                            // core never learns it) — a bare model id is the
                            // best start-time fact, and enough for the
                            // catalog tier.
                            None,
                            model.as_deref(),
                        );
                    }
                    // A head-driven resume (ADR-0112) re-emits `SessionStarted`
                    // for a previously-hibernated child (#609, ADR-0162 §4) — a
                    // no-op for any other session, since a fresh registration is
                    // already `Live` and an untracked id has no entry to update.
                    registry.mark_live(&session);
                    let started_profile = profiles
                        .read()
                        .expect("agent-profile registry lock poisoned")
                        .get(&profile)
                        .cloned();
                    if let Some(p) = started_profile.clone() {
                        active
                            .lock()
                            .expect("active-profile mutex poisoned")
                            .insert(session.clone(), p);
                    }
                    // Ancestor clamp frozen at spawn (#479, ADR-0104 amendment),
                    // mirroring ADR-0024's privilege ceiling for confinement
                    // instead of permission grade.
                    crate::policy::record_session_sandbox(
                        &sandbox.own,
                        &sandbox.floor,
                        &session,
                        parent.as_ref(),
                        started_profile.as_ref().and_then(|p| p.sandbox.as_deref()),
                        sandbox.base,
                    );
                }
                Ok(OutEvent::AgentChanged { session, agent, .. }) => {
                    if let Some(p) = profiles
                        .read()
                        .expect("agent-profile registry lock poisoned")
                        .get(&agent)
                        .cloned()
                    {
                        crate::policy::record_own_sandbox(
                            &sandbox.own,
                            &session,
                            p.sandbox.as_deref(),
                            sandbox.base,
                        );
                        active
                            .lock()
                            .expect("active-profile mutex poisoned")
                            .insert(session, p);
                    }
                }
                // The session's model changed (`SetModel` / a profile pin
                // re-bind, #218/#323). Tool advertising is *not* re-resolved
                // (ADR-0196 §2): the session keeps the mode pinned at start —
                // switching mid-session would bust the prompt cache the mode
                // protects and strand half-emitted history. What this arm
                // does: (a) a start-pair upgrade — a session whose start
                // carried only a bare model id (the startup default's
                // provider is never announced) gets its *first* precise
                // `(provider, model)` pair re-pinned once, which matters only
                // when config is unset and the two lookups could disagree
                // (same id under two providers, one preferring `full`);
                // (b) otherwise the retention notice — same-held-mode plus a
                // log when the new model's catalog preference differs.
                Ok(OutEvent::ModelChanged {
                    session,
                    provider,
                    model,
                    ..
                }) => {
                    if let Some(inputs) = advertising_inputs.as_ref() {
                        let mut modes = advertising
                            .modes
                            .lock()
                            .expect("tool-advertising mode mutex poisoned");
                        if modes.get(&session).is_none() {
                            inputs.pin_session_start(
                                &mut modes,
                                &session,
                                Some(&provider),
                                Some(&model),
                            );
                        } else {
                            inputs.note_model_changed(&modes, &session, &provider, &model);
                        }
                    }
                }
                // A hibernated session (#318) tore down just like an ended one, so
                // its executor-side bookkeeping is equally moot — release it. Its
                // persisted "always" grants survive; a resume rebuilds the rest.
                // The session's live tool overlay changed (#539, ADR-0149):
                // mirror core's full-replacement semantics — an empty list
                // clears the entry entirely.
                Ok(OutEvent::ToolOverlayChanged { session, entries }) => {
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
                    // The sandbox cache (#479) is equally moot — drop both maps.
                    sandbox
                        .own
                        .lock()
                        .expect("sandbox-own mutex poisoned")
                        .remove(&session);
                    sandbox
                        .floor
                        .lock()
                        .expect("sandbox-floor mutex poisoned")
                        .remove(&session);
                    // The plan-file staleness binding (#513) is moot too.
                    plan_files.forget_session(&session);
                    // And the session's pinned tool advertising plus its
                    // discovered-tool set (ADR-0196 §2-3) — a resume re-pins
                    // from its own replayed start pair, and rediscovery is
                    // cheap (the model re-`describe`s what it needs).
                    advertising
                        .modes
                        .lock()
                        .expect("tool-advertising mode mutex poisoned")
                        .forget(&session);
                    advertising
                        .discovered
                        .lock()
                        .expect("discovered-tool mutex poisoned")
                        .forget(&session);
                    // The loop-breaker's last-call tracker (#560, ADR-0196
                    // §6) is equally session-scoped — nothing to break a
                    // loop against once the session is gone.
                    validation.forget(&session);
                }
                // A skill's "active" posture scopes one model turn (#400,
                // ADR-0106; posture-only since ADR-0194): clear it here so a
                // later turn can `load_skill` a different one (or none)
                // cleanly, and tell any listening head via
                // `OutEvent::SkillActive { skill_id: None, .. }`.
                Ok(OutEvent::Done { session, .. }) => {
                    clear_active_skill(&holly, &active_skill, &session);
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
                    // active profile rides on the `ToolExec` itself, so resolve it
                    // from the registry and overwrite the folded entry *before*
                    // any mask/permission decision. The lifecycle fold above is a
                    // lossy broadcast — under burst a dropped
                    // `SessionStarted`/`AgentChanged` would leave a restricted
                    // session unseen and (pre-#156) fail *open*. This makes the
                    // leaf's gate authoritative regardless of that drop; the
                    // fail-closed `permission_for`/`tool_masked` defaults cover
                    // only the residual unknown case (empty/unresolved `agent`).
                    if let Some(p) = profiles
                        .read()
                        .expect("agent-profile registry lock poisoned")
                        .get(&agent)
                        .cloned()
                    {
                        crate::policy::record_own_sandbox(
                            &sandbox.own,
                            &session,
                            p.sandbox.as_deref(),
                            sandbox.base,
                        );
                        active
                            .lock()
                            .expect("active-profile mutex poisoned")
                            .insert(session.clone(), p);
                    }
                    // Physical tool restriction (#116, ADR-0038): a tool outside
                    // the session's effective tool set — its profile's
                    // allowlist/denylist and its session tool overlay,
                    // intersected down the ancestor chain — does not exist for
                    // this agent. Refuse before any other handling (spawn
                    // interception, permission), so a call to a masked
                    // `edit`/`agent` is a hard boundary, not a persona nudge.
                    //
                    // This is now the *whole* restriction, not a backstop:
                    // advertisement is decoupled from enforcement (the model
                    // sees every schema, so the surface stays cache-stable
                    // within a session), which makes an attributed decline
                    // load-bearing — the model has to learn *who* refused it or
                    // it will retry the same call. #597: name which link, and
                    // on whose authority (its profile vs its overlay), since a
                    // child's own definition can list the tool while an
                    // ancestor's narrower mask erases it down the chain.
                    //
                    // `explore`/`describe` are the one deliberate exemption
                    // from this whole walk (#560, ADR-0196 §4): read-only
                    // catalog introspection, never a capability decision — no
                    // profile mask, overlay entry, or ancestor clamp can
                    // withdraw them, mirroring the always-on internal-tool
                    // posture ADR-0190 established for `poll` (subsumed for
                    // `poll` itself by ADR-0192's universal dispatch mask,
                    // but reinstated here narrowly for these two).
                    let masked_by = if is_non_maskable(&tool) {
                        None
                    } else {
                        let active = active.lock().expect("active-profile mutex poisoned");
                        tool_mask_source(&active, &spawn_guard, &overlays, &session, &tool).map(
                            |source| {
                                let name = active.get(&source.session).map(|p| p.name.clone());
                                (source, name)
                            },
                        )
                    };
                    if let Some((source, agent_name)) = masked_by {
                        let holly = holly.clone();
                        let own_session = session.clone();
                        tokio::spawn(async move {
                            let output = crate::decline::mask_decline(
                                &source,
                                &own_session,
                                agent_name.as_deref(),
                                &tool,
                            );
                            seam::reply(&holly, session, request_id, output, true).await;
                        });
                        continue;
                    }
                    // Route the unmasked tool through its interception. The mask
                    // above runs *structurally before* this classifier, and the
                    // routes are a `match` (mutually exclusive) rather than an
                    // ordered ladder of `if tool == X` branches — so no route can
                    // be silently mis-ordered ahead of the mask (#203). Each
                    // handler runs on its own task; the loop only routes.
                    let route = Intercept::classify(&tool);
                    tracing::trace!(
                        %tool,
                        ?route,
                        bypasses_permission = route.bypasses_permission(),
                        "routing tool exec"
                    );
                    match route {
                        Intercept::Spawn => {
                            // Spawn control (#119): the spawner must `may_spawn` and
                            // the *target* must be spawnable and on its allowlist —
                            // refused before a child is minted, in front of the
                            // ADR-0023 budget and the ADR-0024 clamp. Subscribe
                            // *before* handing off so the child's `Done` can't race
                            // ahead of the watcher.
                            let blocking = !crate::subagent::is_background(&input);
                            let target = crate::subagent::target_agent(&input);
                            let refusal = {
                                let active = active.lock().expect("active-profile mutex poisoned");
                                let profiles = profiles
                                    .read()
                                    .expect("agent-profile registry lock poisoned");
                                spawn_refusal(active.get(&session), &target, &profiles)
                            };
                            if let Some(refusal) = refusal {
                                let holly = holly.clone();
                                tokio::spawn(async move {
                                    seam::reply(&holly, session, request_id, refusal, true).await;
                                });
                            } else {
                                match spawn_guard.try_spawn(&session) {
                                    Ok(()) => {
                                        let child_events = holly.subscribe();
                                        let registry = registry.clone();
                                        let retained = retained.clone();
                                        let holly = holly.clone();
                                        tokio::spawn(async move {
                                            // The default blocks and parks for the
                                            // answer; `background: true` hands the
                                            // handle back at once — one guard path,
                                            // two return shapes (#120, #606).
                                            if blocking {
                                                crate::subagent::run_agent(
                                                    holly,
                                                    child_events,
                                                    registry,
                                                    retained,
                                                    session,
                                                    request_id,
                                                    input,
                                                )
                                                .await;
                                            } else {
                                                crate::subagent::launch_subagent(
                                                    holly,
                                                    child_events,
                                                    registry,
                                                    retained,
                                                    session,
                                                    request_id,
                                                    input,
                                                )
                                                .await;
                                            }
                                        });
                                    }
                                    // Over a limit: refuse without starting a child,
                                    // but still answer the parent's parked tool call
                                    // so its turn continues with a clear explanation.
                                    Err(refusal) => {
                                        let holly = holly.clone();
                                        tokio::spawn(async move {
                                            seam::reply(&holly, session, request_id, refusal, true)
                                                .await;
                                        });
                                    }
                                }
                            }
                        }
                        Intercept::AgentSend => {
                            // No spawn-budget/depth gate here (#609): this is a
                            // *reply* into an already-authorized child, not a
                            // new spawn — `AgentRegistry::begin_send` (run
                            // inside the task) is the whole gate: ownership
                            // (only the launching session may send) plus the
                            // lifecycle check (ADR-0162 §4). Subscribe *before*
                            // handing off, mirroring `Spawn`, so the child's
                            // events can't race ahead of the watcher.
                            let child_events = holly.subscribe();
                            let registry = registry.clone();
                            let retained = retained.clone();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::agent_send::run_agent_send(
                                    holly,
                                    child_events,
                                    registry,
                                    retained,
                                    session,
                                    request_id,
                                    input,
                                )
                                .await;
                            });
                        }
                        Intercept::Poll => {
                            let registry = registry.clone();
                            let jobs = jobs.clone();
                            let retained = retained.clone();
                            let scripts = scripts.clone();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::poll::run_poll(
                                    holly, jobs, registry, retained, scripts, session, request_id,
                                    input,
                                )
                                .await;
                            });
                        }
                        Intercept::AskUser => {
                            // Registers with `pending` (and `open_questions`,
                            // #515) before emitting the question (#156), so a
                            // fast answer routes to the parked waiter rather
                            // than racing a per-task broadcast park.
                            let pending = pending.clone();
                            let open_questions = open_questions.clone();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::ask_user::run_ask_user(
                                    holly,
                                    pending,
                                    open_questions,
                                    session,
                                    request_id,
                                    input,
                                )
                                .await;
                            });
                        }
                        Intercept::ProposePlan => {
                            // Approve spawns a sponsored `build` child of the
                            // plan session (ADR-0138). The SpawnGuard mutation
                            // (sponsor check + record) happens synchronously in
                            // this single-threaded loop — race-free — and only
                            // the resolved child id reaches the detached task.
                            let child_events = holly.subscribe();
                            let registry = registry.clone();
                            let retained = retained.clone();
                            let pending = pending.clone();
                            let holly = holly.clone();
                            let plan_files = plan_files.clone();
                            let plan_root = plan_root.clone();
                            // Bound the sponsored spawn (exempt from the per-root
                            // fan-out cap, not from depth). A refusal folds back
                            // as the tool result so the plan turn continues.
                            let sponsored = match spawn_guard.try_sponsor_spawn(&session) {
                                Ok(()) => {
                                    let child = SessionId::new(holly.next_id(IdKind::Session));
                                    spawn_guard
                                        .record_sponsored_start(child.clone(), session.clone());
                                    Some(child)
                                }
                                Err(refusal) => {
                                    let holly = holly.clone();
                                    let sess = session.clone();
                                    let rid = request_id.clone();
                                    tokio::spawn(async move {
                                        seam::reply(&holly, sess, rid, refusal, true).await;
                                    });
                                    None
                                }
                            };
                            if let Some(child) = sponsored {
                                // Registered with `CancelRegistry` (#513): a
                                // `Stop` targeting the plan session aborts this
                                // whole task at any point — the Ask-wait *and*
                                // the post-approval blocking build-wait — with
                                // no reply owed (core cancels the turn on the
                                // same `Stop`). The sponsored build child is a
                                // separate session with its own tasks, so
                                // aborting this one never touches it: "detach"
                                // is simply what an unregistered Stop already
                                // does here. A head wanting to stop the child
                                // too sends it a second, explicit `Stop`.
                                let reg_session = session.clone();
                                let handle = tokio::spawn(async move {
                                    crate::propose_plan::run_propose_plan(
                                        holly,
                                        pending,
                                        registry,
                                        retained,
                                        child_events,
                                        plan_files,
                                        plan_root,
                                        session,
                                        request_id,
                                        input,
                                        child,
                                    )
                                    .await;
                                });
                                cancels.register(
                                    &reg_session,
                                    TaskCanceller::task(handle.abort_handle()),
                                );
                            }
                        }
                        Intercept::Discover => {
                            // Read-only, non-maskable, always-`Allow` (#560,
                            // ADR-0196 §4) — no permission check, no approval
                            // round-trip, just a snapshot read and a reply.
                            let registry_snapshot =
                                tools.read().expect("tool registry lock poisoned").clone();
                            let skills_snapshot =
                                skills.read().expect("skill registry lock poisoned").clone();
                            let mcp_avail = mcp_avail.clone();
                            let mcp_active = mcp_active.clone();
                            let mcp_scopes = mcp_scopes.clone();
                            let advertising = advertising.clone();
                            let holly = holly.clone();
                            if tool == EXPLORE_TOOL {
                                tokio::spawn(async move {
                                    discover::run_explore(
                                        &holly,
                                        &registry_snapshot,
                                        &mcp_avail,
                                        &mcp_active,
                                        skills_snapshot.as_ref(),
                                        session,
                                        request_id,
                                        input,
                                    )
                                    .await;
                                });
                            } else if tool == DESCRIBE_TOOL {
                                tokio::spawn(async move {
                                    discover::run_describe(
                                        &holly,
                                        registry_snapshot,
                                        skills_snapshot.as_ref(),
                                        mcp_scopes.as_deref(),
                                        &advertising,
                                        session,
                                        request_id,
                                        input,
                                    )
                                    .await;
                                });
                            } else {
                                debug_assert_eq!(tool, RESPONSES_TOOL_SEARCH_TOOL);
                                tokio::spawn(async move {
                                    discover::run_tool_search(
                                        &holly,
                                        registry_snapshot,
                                        &mcp_avail,
                                        &mcp_active,
                                        skills_snapshot.as_ref(),
                                        mcp_scopes.as_deref(),
                                        &advertising,
                                        session,
                                        request_id,
                                        input,
                                    )
                                    .await;
                                });
                            }
                        }
                        #[cfg(feature = "rhai")]
                        Intercept::Rhai => {
                            // The bindings resolve permission live against this
                            // loop's profile state — captured here as a per-run
                            // snapshot and moved into the script task. The tool's
                            // *own* Allow/Ask/Deny is resolved the same way. `rhai`
                            // keeps the profile/base path (its inner bindings are a
                            // separate sync mechanism), so it is not routed through
                            // the pluggable resolver (#311); the sync grant read
                            // still upgrades its own `Ask`. The escape-root policy
                            // (ADR-0109) is cloned through too (#446): a file/exec
                            // binding targeting an out-of-root path is gated by the
                            // same forced-`Ask` + `ExtraRootStore` grant as a direct
                            // tool call, not silently hard-refused.
                            // Root-relative arg normalization (#485, ADR-0125):
                            // computed from the escape-root policy before it's
                            // cloned/shadowed below, so an in-root absolute path
                            // grades identically to its relative spelling here too.
                            let arg = grading_arg(
                                &tool,
                                &input,
                                escape_root.as_ref().map(|er| er.root.as_path()),
                            );
                            let escape_root = escape_root.clone();
                            let workdir = crate::permission::permission_workdir(&tool, &input);
                            let (base_self, policy) = {
                                let active = active.lock().expect("active-profile mutex poisoned");
                                let base_self = clamp_to_base(
                                    effective_permission(
                                        &active,
                                        &spawn_guard,
                                        &session,
                                        &tool,
                                        arg.as_deref(),
                                        workdir.as_deref(),
                                    ),
                                    &base,
                                    &tool,
                                    arg.as_deref(),
                                    workdir.as_deref(),
                                );
                                let policy = crate::script::BindingPolicy::capture(
                                    &active,
                                    &spawn_guard,
                                    &overlays,
                                    &session,
                                    &base,
                                    escape_root.as_ref().map(|er| er.root.as_path()),
                                );
                                (base_self, policy)
                            };
                            let self_perm =
                                apply_grant(&*grants, &session, &tool, arg.as_deref(), base_self);
                            let pending = pending.clone();
                            // Snapshot the registry *before* spawning (#372): a brief
                            // read lock, never held across the script's `.await`, so a
                            // concurrent tool registration/removal is invisible to a
                            // script already in flight but picked up by the next one.
                            let tools = tools.read().expect("tool registry lock poisoned").clone();
                            // A scoped session's script sees its scope's cached MCP
                            // tools, never the global set (#684) — cached-only: a
                            // script call must not block this loop on a lazy connect.
                            let tools = match &mcp_scopes {
                                Some(scopes) => scopes.overlay_registry_cached(&session, tools),
                                None => tools,
                            };
                            let holly = holly.clone();
                            // The blocking engine can't be aborted, so pair the
                            // task abort with a cooperative stop flag its progress
                            // callback polls (#167).
                            let stop = Arc::new(AtomicBool::new(false));
                            let reg_session = session.clone();
                            let run_stop = stop.clone();
                            let scripts = scripts.clone();
                            // A `background: true` script (#637, ADR-0185)
                            // deliberately survives a session `Stop`, exactly
                            // as a background `bash`/`call` job does — so it is
                            // never registered with the canceller. Its only
                            // kill is `poll`'s `kill: true`, which trips the
                            // same `stop` flag via the script registry.
                            let background = crate::script::is_background(&input);
                            let handle = tokio::spawn(async move {
                                crate::script::run_rhai(
                                    holly,
                                    tools,
                                    policy,
                                    self_perm,
                                    escape_root,
                                    session,
                                    request_id,
                                    pending,
                                    input,
                                    run_stop,
                                    scripts,
                                )
                                .await;
                            });
                            if !background {
                                cancels.register(
                                    &reg_session,
                                    TaskCanceller::script(handle.abort_handle(), stop),
                                );
                            }
                        }
                        Intercept::Permission => {
                            // Snapshot the ancestor chain *before* spawning so it
                            // stays ordered with the lifecycle events above (and the
                            // `ToolExec.agent` self-heal); the detached task resolves
                            // each session's grade through the pluggable resolver
                            // (#311) and clamps least-privilege across the chain, so
                            // a child sub-agent can never exceed any ancestor (#77).
                            // A root (no ancestors) resolves to its own grade; an
                            // unseen session defaults to `Deny` (fail-closed, #156).
                            // The DB-backed resolver runs in the task, never the loop.
                            let chain = ancestor_chain(&spawn_guard, &session);
                            let resolver = resolver.clone();
                            // The nearest ancestor-chain link with a live
                            // overlay entry for this call (#539, ADR-0149;
                            // `arg_pattern` #611/ADR-0163; chain reach #628),
                            // resolved before spawning like the chain
                            // snapshot above: a matching entry replaces the
                            // profile chain's grade — still ceiling-clamped
                            // inside `dispatch`, which also has the
                            // `arg`/`workdir` an `arg_pattern` rule needs to
                            // resolve against. Walking the whole chain (not
                            // just `session` itself) is what lets a parent's
                            // overlay grade reach a child that has none of
                            // its own, mirroring `tool_mask_source`'s
                            // per-link existence walk.
                            let overlay_entry =
                                crate::permission::overlay_grade_entry(&overlays, &chain, &tool);
                            let ceiling = base.clone();
                            // Snapshot before spawning (#372) — see the Rhai arm above.
                            let tools = tools.read().expect("tool registry lock poisoned").clone();
                            let holly = holly.clone();
                            let skills = skills.clone();
                            let active_skill = active_skill.clone();
                            let grants = grants.clone();
                            let hooks = hooks.clone();
                            let pending = pending.clone();
                            let escape_root = escape_root.clone();
                            let mcp_scopes = mcp_scopes.clone();
                            let advertising = advertising.clone();
                            let validation = validation.clone();
                            // Register so a `Stop` aborts this task mid-execution:
                            // aborting the future drops the exec tool's child,
                            // firing its process-group SIGKILL guard (#167/#168).
                            let reg_session = session.clone();
                            let handle = tokio::spawn(async move {
                                // A scoped session's snapshot swaps its `mcp__*`
                                // namespace for the scope's own tools (#684),
                                // lazily connecting the called server with the
                                // scope's credentials — in the detached task,
                                // never this loop. A refusal (auth-required,
                                // connect failure) is the call's tool error.
                                let tools = match &mcp_scopes {
                                    Some(scopes) => {
                                        match scopes
                                            .overlay_registry_for_call(&session, tools, &tool)
                                            .await
                                        {
                                            Ok(tools) => tools,
                                            Err(msg) => {
                                                seam::reply(&holly, session, request_id, msg, true)
                                                    .await;
                                                return;
                                            }
                                        }
                                    }
                                    None => tools,
                                };
                                dispatch(
                                    &holly,
                                    &tools,
                                    &skills,
                                    &active_skill,
                                    &*resolver,
                                    &chain,
                                    &*grants,
                                    &hooks,
                                    &pending,
                                    escape_root.as_ref(),
                                    overlay_entry,
                                    &ceiling,
                                    &advertising,
                                    &validation,
                                    session,
                                    request_id,
                                    tool,
                                    input,
                                )
                                .await;
                            });
                            cancels
                                .register(&reg_session, TaskCanceller::task(handle.abort_handle()));
                        }
                    }
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

/// Resolve one `ToolExec` per its permission and reply with a `ToolResult`.
///
/// The grade comes from the pluggable [`PermissionResolver`] (#311), clamped
/// least-privilege across the call's ancestor `chain` (the sub-agent ceiling,
/// ADR-0024) and upgraded from `Ask` to `Allow` by an existing [`GrantStore`]
/// grant. The DB-backed resolve runs here in the detached task, not the loop.
///
/// A `pre_tool_use` hook (#199) can **veto** the call: a non-zero-exit hook
/// short-circuits with a denial `ToolResult`, so the tool neither prompts nor
/// runs. Cleared hooks fall through to the normal `Allow | Ask | Deny` dispatch.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    resolver: &dyn PermissionResolver,
    chain: &[SessionId],
    grants: &dyn GrantStore,
    hooks: &Hooks,
    pending: &crate::pending::PendingDecisions,
    escape_root: Option<&EscapeRoot>,
    // The nearest ancestor-chain link's live overlay entry for this call
    // (#539, ADR-0149; `arg_pattern` #611/ADR-0163; chain reach #628):
    // `Some` when a `ToolOverlayEntry` matched the tool at `session` or one
    // of its ancestors — [`overlay_entry_grade`][crate::permission::overlay_entry_grade]
    // materializes it into a profile that replaces the profile chain's grade
    // (that override is the overlay's point; the injecting head is trusted),
    // clamped against `ceiling` below so the config permission ceiling (#172)
    // still wins.
    overlay_entry: Option<entanglement_core::ToolOverlayEntry>,
    ceiling: &PermissionProfile,
    // Pre-dispatch argument-validation state (#560, ADR-0196 §6): the
    // delivered-schema dedup (shares `advertising.discovered` with `describe`,
    // ADR-0196 §4) and the loop-breaker's per-session last-call tracker.
    advertising: &tool_advertising::AdvertisingState,
    validation: &arg_validate::LoopBreaker,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    // A hallucinated tool name can never execute, so reject it *before* the
    // ladder runs (#437): otherwise an `Ask` grade prompts the user to approve
    // a call that can only fail, `pre_tool_use` vetoes a call that was never
    // executable, and an `Always`-scoped approval could record a grant for a
    // tool that doesn't exist. Uses the same freshly-snapshotted `tools`
    // `dispatch` already received, so a live `McpAdd`/`McpRemove` (#372) is
    // honored exactly as execution itself would see it. `update_tasks` is a
    // runtime state tool with no registry entry (#231, ADR-0049) —
    // `run_and_reply` handles it separately — so it's exempt from this
    // registry check.
    if !tools.contains(&tool) && !crate::plan_tasks::is_state_tool(&tool) {
        let output = tools.unknown_tool_message(&tool);
        seam::reply(holly, session, request_id, output, true).await;
        return;
    }
    // Alias rewrite (#560 P8): a skill-declared alias — a renamed/preset-args
    // wrapper over another tool, or the rewrite-to-`rhai` a rhai-backed skill
    // tool is sugar for — must not launder permission through its own
    // namespaced name (`Tool::alias_rewrite`'s doc). Rewriting *here*, before
    // grading/escape-root/hooks/the approval round-trip all run, means every
    // one of them operates on the wrapped tool's real name and merged input —
    // exactly as if the model had called it directly — with zero special-
    // casing anywhere else in this pipeline. A tool that never aliases
    // (everything but `skills::alias_tool::AliasTool`) leaves `(tool, input)`
    // untouched.
    let (tool, input) = match tools.get(&tool).and_then(|t| t.alias_rewrite(&input)) {
        Some(rewritten) => rewritten,
        None => (tool, input),
    };
    // Resolve + apply grants first (matching the pre-seam order where `perm` was
    // computed before the hook ran), so a grant upgrade and the veto compose the
    // same way. The tool-specific argument (command/path, #173) lets an
    // argument-scoped rule resolve against the call. Normalized root-relative
    // (#485, ADR-0125) so an in-root absolute path grades identically to its
    // relative spelling and keys the same grant — computed once here and
    // threaded into `await_decision` below instead of recomputed there, so the
    // grant lookup (`apply_grant`) and grant record (on approval) provably
    // share one key.
    let arg = grading_arg(&tool, &input, escape_root.map(|er| er.root.as_path()));
    let workdir = crate::permission::permission_workdir(&tool, &input);
    let base_perm = match overlay_entry {
        Some(entry) => {
            let grade = crate::permission::overlay_entry_grade(&tool, &entry).resolve_scoped(
                &tool,
                arg.as_deref(),
                workdir.as_deref(),
            );
            clamp_to_base(grade, ceiling, &tool, arg.as_deref(), workdir.as_deref())
        }
        None => resolve_effective(resolver, chain, &tool, &input).await,
    };
    let perm = apply_grant(grants, &session, &tool, arg.as_deref(), base_perm);
    if let Some(reason) = hooks.run_pre_tool_use(&session, &tool, &input).await {
        seam::reply(holly, session, request_id, reason, true).await;
        return;
    }
    // Escape-root gate (ADR-0109): a `read`/`edit`/`write` path or `bash`/`call`
    // `workdir` that resolves *outside* the project root requires explicit
    // approval — even when the profile would `Allow` — unless the user already
    // durably granted this exact `(tool, path)`. A `Deny` floor still wins (the
    // profile forbade the tool outright), so escaping never *lowers* the bar.
    // `None` (no escape policy wired) is the pre-ADR-0109 strict-containment path.
    let escape = escape_root
        .filter(|_| perm != Permission::Deny)
        .and_then(|er| er.escaping(&tool, &input).map(|abs| (er, abs)))
        .filter(|(er, abs)| !er.store.is_durably_allowed(&tool, abs));

    match perm {
        Permission::Allow if escape.is_none() => {
            run_and_reply(
                holly,
                tools,
                skills,
                active_skill,
                hooks,
                advertising,
                validation,
                session,
                request_id,
                tool,
                input,
            )
            .await;
        }
        Permission::Deny => {
            let output = format!("tool `{tool}` denied by permission profile");
            seam::reply(holly, session, request_id, output, true).await;
        }
        // Either the profile said `Ask`, or an out-of-root access forced one.
        _ => {
            // Register the waiter *before* prompting (#156) so the inbound router
            // can never process the approval before this park exists — the
            // lag-proof successor to the old "subscribe before prompting"
            // discipline. The prompt mints a **fresh** per-session seq (#157) from
            // the parked session's shared counter, so `(session, seq)` stays unique
            // instead of reusing the `ToolExec` seq.
            let rx = pending.register(&session, &request_id);
            let escape_grant = escape.map(|(er, abs)| (er.store.clone(), abs));
            holly.emit_for_session(&session, |seq| OutEvent::ToolRequest {
                session: session.clone(),
                seq,
                request_id: request_id.clone(),
                tool: tool.clone(),
                input: escape_grant
                    .as_ref()
                    .map(|(_, abs)| {
                        format!(
                            "{input}\n\n⚠ accesses a path OUTSIDE the project root: {}",
                            abs.display()
                        )
                    })
                    .unwrap_or_else(|| input.clone()),
            });
            holly.emit_status(&session, AgentState::WaitingApproval);
            await_decision(
                holly,
                tools,
                skills,
                active_skill,
                grants,
                hooks,
                advertising,
                validation,
                rx,
                escape_grant,
                session,
                request_id,
                tool,
                input,
                arg,
            )
            .await;
        }
    }
}

/// Park until the head answers the pending approval, then run-or-refuse. A
/// `Stop` (Esc-in-approval) unwinds silently: core's `wait_tool_result` sees the
/// same `Stop` on its inbox and cancels the turn, so no `ToolResult` is owed
/// (the shared park/filter is [`crate::seam::await_decision`]). `arg` is the
/// grading-time argument `dispatch` already computed (#485, ADR-0125) — taken
/// as a parameter rather than recomputed here, so the grant this records on
/// approval provably uses the exact same key `apply_grant` looked up before
/// the prompt was ever shown.
#[allow(clippy::too_many_arguments)]
async fn await_decision(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    grants: &dyn GrantStore,
    hooks: &Hooks,
    advertising: &tool_advertising::AdvertisingState,
    validation: &arg_validate::LoopBreaker,
    rx: tokio::sync::oneshot::Receiver<seam::Decision>,
    escape_grant: Option<(Arc<crate::extra_roots::ExtraRootStore>, std::path::PathBuf)>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
    arg: Option<String>,
) {
    match crate::pending::await_decision(rx).await {
        seam::Decision::Approve { scope } => {
            set_thinking(holly, &session);
            if let Some((store, abs)) = &escape_grant {
                // The prompt was forced by an out-of-root access (ADR-0109):
                // record the approval in the escape-root store so the host tool's
                // containment check lets *this tool* reach *this path*. Every scope
                // is recorded (a `Once` becomes the single-use token bound to this
                // exact `request_id`, #449, so a concurrent call to the same path
                // can't consume it); `Session`/`Always` also relax future
                // containment and let the executor skip re-asking. Per-tool by
                // construction.
                store.record(&tool, abs, scope, &request_id);
            } else if scope != ApprovalScope::Once {
                // Ordinary (in-root) approval: record the wider scopes (#174) so an
                // identical later call skips this prompt — through the pluggable
                // [`GrantStore`] (#311). `Once` records nothing.
                grants.record(&session, &tool, arg.as_deref(), scope).await;
            }
            run_and_reply(
                holly,
                tools,
                skills,
                active_skill,
                hooks,
                advertising,
                validation,
                session,
                request_id,
                tool,
                input,
            )
            .await;
        }
        seam::Decision::Reject { reason } => {
            set_thinking(holly, &session);
            let output = format!(
                "tool `{tool}` rejected: {}",
                reason.as_deref().unwrap_or("user")
            );
            seam::reply(holly, session, request_id, output, true).await;
        }
        // `Stop` (and a closed inbox) unwind silently; `Answer`/`Retract`/
        // `Replace` never target a tool-approval request id (they are
        // `ask_user`-only, #515).
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_and_reply(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    hooks: &Hooks,
    advertising: &tool_advertising::AdvertisingState,
    validation: &arg_validate::LoopBreaker,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    // `update_tasks` carries no host resource (#231, ADR-0049): it is not in
    // the registry. The runtime emits its `TaskList` snapshot — minting a
    // **fresh** per-session seq (#157) so it takes an ordered place in the
    // content stream instead of colliding with the parked `ToolExec` seq —
    // and acks (text), instead of dispatching.
    if crate::plan_tasks::is_state_tool(&tool) {
        holly.emit_for_session(&session, |seq| {
            crate::plan_tasks::state_event(&session, seq, &tool, &input)
                .expect("is_state_tool ⇒ state_event is Some")
        });
        let ack = crate::plan_tasks::ack(&tool);
        hooks
            .run_post_tool_use(&session, &tool, &input, &ack, false, None)
            .await;
        seam::reply(holly, session, request_id, ack, false).await;
        return;
    }
    // Pre-dispatch argument validation (#560, ADR-0196 §6): a call whose
    // input violates the tool's advertised schema (missing/unexpected
    // properties, a type mismatch) never reaches `Tool::run` at all — it gets
    // a specific complaint instead of `run()`'s opaque parse-failure text.
    // A tool absent from the registry (a runtime-owned pseudo-tool like
    // `update_tasks`/`ask_user`/`poll`, already handled above or dispatched
    // elsewhere) has no advertised schema here, so it's exempt by
    // construction — nothing to validate against.
    if let Some(spec) = tools.spec_for(&tool) {
        if let Some(violation) = arg_validate::validate(&spec.schema, &input) {
            tracing::warn!(
                tool = %tool,
                violation = ?violation.lines(),
                "tool call failed pre-dispatch schema validation"
            );
            let already_delivered = advertising
                .discovered
                .lock()
                .expect("discovered-tool mutex poisoned")
                .contains(&session, &tool);
            if !already_delivered {
                advertising
                    .discovered
                    .lock()
                    .expect("discovered-tool mutex poisoned")
                    .mark(&session, &tool);
            }
            let mut output = arg_validate::decline_text(&spec, &violation, already_delivered);
            if validation.note(&session, &tool, &input, true) {
                output.push_str("\n\n");
                output.push_str(arg_validate::LOOP_BREAKER_NOTE);
            }
            hooks
                .run_post_tool_use(&session, &tool, &input, &output, true, None)
                .await;
            seam::reply(holly, session, request_id, output, true).await;
            return;
        }
    }
    // Every other tool executes against the host registry, returning multimodal
    // content (a text result, or an image block for `read` on an image, #221)
    // plus `is_error` (#636, ADR-0176). `edit`/`write` record their change into
    // the capture scope (#202); the executor mints a fresh `FileChange` seq
    // (#157) and broadcasts the audit event before replying with the
    // `ToolResult`. `duration_ms` is measured around the whole execution —
    // generic across every host tool, unlike `is_error` which the registry
    // itself classifies — so a slow `bash`/`call` is visible without either
    // parsing its `[exit N]` header or threading a bespoke timer through every
    // `Tool` impl.
    let started = std::time::Instant::now();
    let execution = crate::file_change::capture_and_emit(
        holly,
        &session,
        tools.execute(
            &ToolCall {
                id: request_id.clone(),
                name: tool.clone(),
                input: input.clone(),
                provider_meta: None,
            },
            &session,
        ),
    )
    .await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ToolExecution {
        mut content,
        is_error,
        exit_code,
    } = execution;
    let mut output_text = entanglement_core::content_text(&content);
    // Loop-breaker guard (#560, ADR-0196 §6): two identical failing calls in a
    // row — same tool, same input, both `is_error` — mean the schema was
    // never the problem. Deliberately generic across every failure kind
    // (unknown-tool and schema-violation return earlier, above/in `dispatch`;
    // this covers a runtime tool error and an MCP required-param rejection
    // alike). A non-error result never triggers the note, matching decision
    // 3: a command failure (non-zero exit) is untouched.
    if validation.note(&session, &tool, &input, is_error) && is_error {
        output_text.push_str("\n\n");
        output_text.push_str(arg_validate::LOOP_BREAKER_NOTE);
        content.push(entanglement_core::ContentPart::text(format!(
            "\n\n{}",
            arg_validate::LOOP_BREAKER_NOTE
        )));
    }
    // #400, ADR-0106 (posture-only since ADR-0194): a successful `load_skill`
    // records the session's skill-active posture and tells any listening head
    // via `OutEvent::SkillActive` — parsed from the result's `skill_id:`
    // header (absent on a failed load: unknown/`user_only` skill, which
    // leaves any prior posture untouched). It no longer narrows the
    // session's tool set.
    if tool == LOAD_SKILL_TOOL {
        activate_skill(holly, skills, active_skill, &session, &output_text);
    }
    // `post_tool_use` (#199) observes the result before it is folded back — a
    // pure side-effect (formatter/telemetry); it cannot rewrite `content`, but
    // it now also observes `is_error` (#636) so a hook can branch on outcome
    // without re-parsing `output`.
    hooks
        .run_post_tool_use(&session, &tool, &input, &output_text, is_error, exit_code)
        .await;
    seam::reply_content(
        holly,
        session,
        request_id,
        content,
        is_error,
        Some(duration_ms),
        exit_code,
    )
    .await;
}

/// Record `session`'s skill-active posture (#400, ADR-0106; posture-only
/// since ADR-0194 — skills no longer mask tools) from a `load_skill` result:
/// parse its `skill_id:` header, look the skill up in the live registry for
/// its (now-vestigial, wire-compat-only) `allowed_tools`, and tell any
/// listening head via [`OutEvent::SkillActive`]. A `result` with no
/// `skill_id:` header (a failed load) is a no-op — the session keeps
/// whatever posture was active before.
fn activate_skill(
    holly: &Holly,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    session: &SessionId,
    result: &str,
) {
    let Some(skill_id) = parse_skill_id(result) else {
        return;
    };
    let allowed_tools = skills
        .read()
        .expect("skill registry lock poisoned")
        .get(skill_id)
        .and_then(|s| s.allowed_tools.clone());
    active_skill
        .lock()
        .expect("active-skill mutex poisoned")
        .insert(session.clone());
    holly.emit_for_session(session, |seq| OutEvent::SkillActive {
        session: session.clone(),
        seq,
        skill_id: Some(skill_id.to_string()),
        allowed_tools,
    });
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

fn set_thinking(holly: &Holly, session: &SessionId) {
    holly.emit_status(session, AgentState::Thinking);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_each_orchestration_tool_to_its_route() {
        assert_eq!(Intercept::classify(AGENT_TOOL), Intercept::Spawn);
        assert_eq!(Intercept::classify(AGENT_SEND_TOOL), Intercept::AgentSend);
        assert_eq!(Intercept::classify(POLL_TOOL), Intercept::Poll);
        assert_eq!(Intercept::classify(ASK_USER_TOOL), Intercept::AskUser);
        assert_eq!(
            Intercept::classify(PROPOSE_PLAN_TOOL),
            Intercept::ProposePlan
        );
        assert_eq!(Intercept::classify(EXPLORE_TOOL), Intercept::Discover);
        assert_eq!(Intercept::classify(DESCRIBE_TOOL), Intercept::Discover);
        assert_eq!(
            Intercept::classify(RESPONSES_TOOL_SEARCH_TOOL),
            Intercept::Discover
        );
        #[cfg(feature = "rhai")]
        assert_eq!(Intercept::classify(RHAI_TOOL), Intercept::Rhai);
    }

    #[test]
    fn classify_routes_every_other_tool_to_permission() {
        // Host-registry tools and runtime state tools take the generic path.
        for tool in [
            "read",
            "write",
            "edit",
            "bash",
            "call",
            crate::tool_names::UPDATE_TASKS_TOOL,
            "",
        ] {
            assert_eq!(
                Intercept::classify(tool),
                Intercept::Permission,
                "`{tool}` should fall through to the permission dispatch"
            );
        }
    }

    #[test]
    fn only_orchestration_routes_bypass_permission() {
        // The spawn/poll/prompt/plan routes touch no host resource; `rhai`
        // resolves permission itself and the generic path *is* the decision.
        assert!(Intercept::Spawn.bypasses_permission());
        assert!(Intercept::AgentSend.bypasses_permission());
        assert!(Intercept::Poll.bypasses_permission());
        assert!(Intercept::AskUser.bypasses_permission());
        assert!(Intercept::ProposePlan.bypasses_permission());
        assert!(Intercept::Discover.bypasses_permission());
        #[cfg(feature = "rhai")]
        assert!(!Intercept::Rhai.bypasses_permission());
        assert!(!Intercept::Permission.bypasses_permission());
    }

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

    /// A resolver that answers a fixed grade per session id (default `Allow`),
    /// so a test can prove the executor's ancestor clamp (#311, ADR-0024) sits
    /// *on top of* the pluggable resolver.
    struct PerSessionResolver(std::collections::HashMap<SessionId, Permission>);

    #[async_trait::async_trait]
    impl PermissionResolver for PerSessionResolver {
        async fn resolve(&self, session: &SessionId, _tool: &str, _input: &str) -> Permission {
            self.0.get(session).copied().unwrap_or(Permission::Allow)
        }
    }

    #[tokio::test]
    async fn resolve_effective_clamps_least_privilege_over_the_chain() {
        let child = SessionId::new("child");
        let parent = SessionId::new("parent");
        // The tenant rule *widens* the child to Allow, but its parent resolves
        // Ask — the chain min must clamp the child back to Ask, so a resolver can
        // never widen a sub-agent beyond its ancestor.
        let resolver = PerSessionResolver(
            [
                (child.clone(), Permission::Allow),
                (parent.clone(), Permission::Ask),
            ]
            .into_iter()
            .collect(),
        );
        let chain = vec![child.clone(), parent.clone()];
        assert_eq!(
            resolve_effective(&resolver, &chain, "bash", "{}").await,
            Permission::Ask
        );
        // A root (single-element chain) resolves to its own grade unchanged.
        assert_eq!(
            resolve_effective(&resolver, std::slice::from_ref(&child), "bash", "{}").await,
            Permission::Allow
        );
        // A parent `Deny` floors the child regardless of the tenant's Allow.
        let deny_parent =
            PerSessionResolver([(parent.clone(), Permission::Deny)].into_iter().collect());
        assert_eq!(
            resolve_effective(&deny_parent, &chain, "bash", "{}").await,
            Permission::Deny
        );
    }
}
