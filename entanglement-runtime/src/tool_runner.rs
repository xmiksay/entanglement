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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{
    AgentProfile, AgentState, ApprovalScope, Holly, InMsg, OutEvent, Permission, PermissionProfile,
    ProfileRegistry, SessionId, ToolCall,
};

use crate::tools::{SharedRegistry, ToolRegistry};
use tokio::sync::broadcast::error::RecvError;

use crate::cancel::{CancelRegistry, TaskCanceller};
use crate::hooks::Hooks;
use crate::permission::{
    ancestor_chain, clamp_to_base, effective_permission, min_permission, permission_arg,
    skill_masked, spawn_refusal, tool_masked, ActiveSkill,
};
use crate::policy::{DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver};
use crate::seam;
use crate::skills::load_skill::parse_skill_id;
use crate::skills::SkillRegistry;
use crate::tool_names::{
    AGENT_POLL_TOOL, AGENT_SPAWN_TOOL, AGENT_TOOL, ASK_USER_TOOL, LOAD_SKILL_TOOL,
    PROPOSE_PLAN_TOOL, RHAI_TOOL,
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
    /// `agent`/`agent_spawn`: session orchestration only (touches no host
    /// resource), gated by the per-profile spawn control, not per-tool approval
    /// (#60/#119/#120). The two variants share one guard path and differ only in
    /// whether the launch blocks for the answer.
    Spawn,
    /// `agent_poll`: the join half of a non-blocking spawn (#89, ADR-0026) — it
    /// reads accumulated spawn state, starting no session and touching no host.
    AgentPoll,
    /// `ask_user`: a runtime-owned prompt tool (#90, ADR-0027) that surfaces a
    /// question to the head instead of running against the registry.
    AskUser,
    /// `propose_plan`: the plan agent's finalize step (#141, ADR-0042),
    /// force-parked on the `Ask` path since user approval *is* its semantics.
    ProposePlan,
    /// `rhai`: a sandboxed script tool (#122, ADR-0046) that resolves its own
    /// permission live against the loop's profile snapshot inside the script task.
    Rhai,
    /// Every other host tool: the generic `Allow | Ask | Deny` dispatch.
    Permission,
}

impl Intercept {
    /// Route an (already-unmasked) tool by name.
    fn classify(tool: &str) -> Self {
        match tool {
            AGENT_TOOL | AGENT_SPAWN_TOOL => Self::Spawn,
            AGENT_POLL_TOOL => Self::AgentPoll,
            ASK_USER_TOOL => Self::AskUser,
            PROPOSE_PLAN_TOOL => Self::ProposePlan,
            RHAI_TOOL => Self::Rhai,
            _ => Self::Permission,
        }
    }

    /// Whether this route skips the per-tool `Allow | Ask | Deny` decision. The
    /// spawn/poll/prompt/plan routes touch no host resource, so permission does
    /// not apply; `Rhai` resolves permission itself inside the script task; the
    /// generic `Permission` route *is* the permission decision.
    fn bypasses_permission(self) -> bool {
        matches!(
            self,
            Self::Spawn | Self::AgentPoll | Self::AskUser | Self::ProposePlan
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
    let resolver: Arc<dyn PermissionResolver> =
        Arc::new(ProfileResolver::new(active.clone(), base.clone()));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    spawn_tool_executor_with_policy(
        holly,
        tools.shared(),
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
/// executor parses the `skill_id` its result carries, looks up that skill's
/// `allowed_tools` here, and activates the session's skill mask —
/// [`skill_masked`], layered after the #116 agent mask.
/// Escape-root policy for the executor (ADR-0109): the canonical project `root`
/// against which an out-of-root `read`/`edit`/`write` path or `bash`/`call`
/// `workdir` is detected, plus the shared [`ExtraRootStore`] approvals are
/// recorded into and the host tools read. `None` (the wrappers below, all tests)
/// keeps strict containment — an out-of-root path is a hard error, never a
/// prompt.
#[derive(Clone)]
pub struct EscapeRoot {
    pub root: std::path::PathBuf,
    pub store: Arc<crate::extra_roots::ExtraRootStore>,
}

impl EscapeRoot {
    /// The absolute out-of-root path a call to `tool` with `input` would touch,
    /// or `None` when it stays contained (or the tool has no path argument).
    fn escaping(&self, tool: &str, input: &str) -> Option<std::path::PathBuf> {
        let rel = crate::permission::escape_root_target(tool, input)?;
        crate::host::escaping_path(&self.root, &rel)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_tool_executor_with_policy(
    holly: &Holly,
    tools: SharedRegistry,
    profiles: Arc<RwLock<ProfileRegistry>>,
    skills: Arc<RwLock<Arc<SkillRegistry>>>,
    base: PermissionProfile,
    active: Arc<Mutex<HashMap<SessionId, AgentProfile>>>,
    resolver: Arc<dyn PermissionResolver>,
    grants: Arc<dyn GrantStore>,
    hooks: Hooks,
    escape_root: Option<EscapeRoot>,
) -> tokio::task::JoinHandle<()> {
    let hooks = Arc::new(hooks);
    let mut sub = holly.subscribe();
    // Subscribe to the inbound fan-out *synchronously*, before this function
    // returns, so a `Prompt`/`Stop` the caller sends right after spawning can't
    // race ahead of the watcher's subscription (the `user_prompt_submit` hook,
    // #199, depends on catching that first prompt).
    let inbound = holly.subscribe_inbound();
    let holly = holly.clone();
    tokio::spawn(async move {
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
        // The session's active-skill tool mask (#400, ADR-0106): set when a
        // `load_skill` call resolves with an `allowed_tools` list, layered after
        // the #116 agent mask below (`skill_masked`). Cleared on the turn's
        // `Done` — a skill's scope is one conversational turn — or when the
        // session ends. Shared with `dispatch`/`await_decision`/`run_and_reply`
        // (the detached per-call tasks), which set it after a successful
        // `load_skill`; this loop is the sole writer of the clear path.
        let active_skill: Arc<Mutex<HashMap<SessionId, ActiveSkill>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // Bounds the spawn tree (#76): tracks parent links from lifecycle events
        // and per-root spawn budgets. Lives in this single-threaded loop, so the
        // spawn decision below is race-free.
        let mut spawn_guard = crate::subagent::SpawnGuard::new();
        // Answer + timing per launched sub-agent, keyed by its handle (#89).
        // Shared with the detached launch watchers and `agent_poll` tasks.
        let registry = crate::agent_poll::AgentRegistry::default();
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
        // Lag-proof decision delivery (#156): parked approvals await a oneshot
        // registered here, and the single inbound router below fans each head
        // decision to its waiter — closing the window where a per-task broadcast
        // park lagged and silently dropped an `Approve`/`Reject`/`Answer`.
        let pending = crate::pending::PendingDecisions::default();
        {
            // The single inbound router (#156): the *sole* consumer of the inbound
            // fan-out for decisions. It watches `Stop` (cancel in-flight tools +
            // unwind parked approvals, #167), fires the `user_prompt_submit` hooks
            // (#199) off each `Prompt`, and resolves every `Approve`/`Reject`/
            // `Answer` to its parked waiter. One light map-lookup-per-frame loop
            // drains far faster than a park loop, so it does not lag the way the
            // per-task subscriptions it replaced did.
            let cancels = cancels.clone();
            let hooks = hooks.clone();
            let pending = pending.clone();
            let mut inbound = inbound;
            tokio::spawn(async move {
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
                    ..
                }) => {
                    spawn_guard.record_start(session.clone(), parent);
                    if let Some(p) = profiles.read().unwrap().get(&profile).cloned() {
                        active.lock().unwrap().insert(session, p);
                    }
                }
                Ok(OutEvent::AgentChanged { session, agent, .. }) => {
                    if let Some(p) = profiles.read().unwrap().get(&agent).cloned() {
                        active.lock().unwrap().insert(session, p);
                    }
                }
                // A hibernated session (#318) tore down just like an ended one, so
                // its executor-side bookkeeping is equally moot — release it. Its
                // persisted "always" grants survive; a resume rebuilds the rest.
                Ok(OutEvent::SessionEnded { session, .. })
                | Ok(OutEvent::SessionHibernated { session, .. }) => {
                    // Drop the closed session's in-memory grants (#174); persisted
                    // "always" grants survive.
                    grants.forget_session(&session);
                    // Its in-flight tool bookkeeping is moot once the session ends.
                    cancels.forget_session(&session);
                    // Drop the re-offer dedupe set (#274): its request ids can
                    // never recur once the session is gone.
                    in_flight.remove(&session);
                    // The active-skill mask is moot once the session is gone too
                    // (#400) — no `Done` will follow to clear it otherwise.
                    active_skill.lock().unwrap().remove(&session);
                }
                // A skill's tool mask scopes one model turn (#400, ADR-0106):
                // clear it here so a later turn can `load_skill` a different one
                // (or none) unmasked, and tell any listening head the combined
                // posture reverted to just the #116 agent mask.
                Ok(OutEvent::Done { session, .. }) => {
                    clear_active_skill(&holly, &active_skill, &session);
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
                    if let Some(p) = profiles.read().unwrap().get(&agent).cloned() {
                        active.lock().unwrap().insert(session.clone(), p);
                    }
                    // Physical tool restriction (#116, ADR-0038): a tool outside
                    // the session's effective advertised set — its profile's
                    // allowlist/denylist, intersected down the ancestor chain —
                    // does not exist for this agent. Refuse before any other
                    // handling (spawn interception, permission), so even a
                    // hallucinated call to a masked `edit`/`agent_spawn` is a
                    // hard boundary, not a persona nudge. Core already withholds
                    // the schema; this closes the gap if the model calls it anyway.
                    let masked = {
                        let active = active.lock().unwrap();
                        tool_masked(&active, &spawn_guard, &session, &tool)
                    };
                    if masked {
                        let holly = holly.clone();
                        tokio::spawn(async move {
                            let output =
                                format!("tool `{tool}` is not available to this agent (restricted by profile)");
                            seam::reply(&holly, session, request_id, output).await;
                        });
                        continue;
                    }
                    // Skill-scoped tool restriction (#400, ADR-0106), layered
                    // *after* the #116 agent mask above — a tool must survive
                    // both. A loaded skill's `allowed_tools` narrows the session's
                    // already-unmasked set for the rest of this turn.
                    let skill_masked_by = {
                        let active_skill = active_skill.lock().unwrap();
                        skill_masked(&active_skill, &session, &tool)
                    };
                    if let Some(skill_id) = skill_masked_by {
                        let holly = holly.clone();
                        tokio::spawn(async move {
                            let output = format!(
                                "tool `{tool}` is not available while skill `{skill_id}` is \
                                 active (restricted by its allowed_tools)"
                            );
                            seam::reply(&holly, session, request_id, output).await;
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
                            let blocking = tool == AGENT_TOOL;
                            let target = crate::subagent::target_agent(&input);
                            let refusal = {
                                let active = active.lock().unwrap();
                                let profiles = profiles.read().unwrap();
                                spawn_refusal(active.get(&session), &target, &profiles)
                            };
                            if let Some(refusal) = refusal {
                                let holly = holly.clone();
                                tokio::spawn(async move {
                                    seam::reply(&holly, session, request_id, refusal).await;
                                });
                            } else {
                                match spawn_guard.try_spawn(&session) {
                                    Ok(()) => {
                                        let child_events = holly.subscribe();
                                        let registry = registry.clone();
                                        let holly = holly.clone();
                                        tokio::spawn(async move {
                                            // `agent` parks for the answer; `agent_spawn`
                                            // hands the handle back at once — one guard
                                            // path, two return shapes (#120).
                                            if blocking {
                                                crate::subagent::run_agent(
                                                    holly,
                                                    child_events,
                                                    registry,
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
                                            seam::reply(&holly, session, request_id, refusal).await;
                                        });
                                    }
                                }
                            }
                        }
                        Intercept::AgentPoll => {
                            let registry = registry.clone();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::agent_poll::run_agent_poll(
                                    holly, registry, session, request_id, input,
                                )
                                .await;
                            });
                        }
                        Intercept::AskUser => {
                            // Registers with `pending` before emitting the question
                            // (#156), so a fast answer routes to the parked waiter
                            // rather than racing a per-task broadcast park.
                            let pending = pending.clone();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::ask_user::run_ask_user(
                                    holly, pending, session, request_id, input,
                                )
                                .await;
                            });
                        }
                        Intercept::ProposePlan => {
                            // Approve just acks the model (no engine plan state,
                            // #231); the head handles the fresh-`build`-session
                            // handoff (head policy, no new protocol surface).
                            let pending = pending.clone();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::propose_plan::run_propose_plan(
                                    holly, pending, session, request_id, input,
                                )
                                .await;
                            });
                        }
                        Intercept::Rhai => {
                            // The bindings resolve permission live against this
                            // loop's profile state — captured here as a per-run
                            // snapshot and moved into the script task. The tool's
                            // *own* Allow/Ask/Deny is resolved the same way. `rhai`
                            // keeps the profile/base path (its inner bindings are a
                            // separate sync mechanism), so it is not routed through
                            // the pluggable resolver (#311); the sync grant read
                            // still upgrades its own `Ask`.
                            let arg = permission_arg(&tool, &input);
                            let (base_self, policy) = {
                                let active = active.lock().unwrap();
                                let base_self = clamp_to_base(
                                    effective_permission(
                                        &active,
                                        &spawn_guard,
                                        &session,
                                        &tool,
                                        arg.as_deref(),
                                    ),
                                    &base,
                                    &tool,
                                    arg.as_deref(),
                                );
                                let policy = crate::script::BindingPolicy::capture(
                                    &active,
                                    &spawn_guard,
                                    &session,
                                    &base,
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
                            let tools = tools.read().unwrap().clone();
                            let holly = holly.clone();
                            // The blocking engine can't be aborted, so pair the
                            // task abort with a cooperative stop flag its progress
                            // callback polls (#167).
                            let stop = Arc::new(AtomicBool::new(false));
                            let reg_session = session.clone();
                            let run_stop = stop.clone();
                            let handle = tokio::spawn(async move {
                                crate::script::run_rhai(
                                    holly, tools, policy, self_perm, session, request_id, pending,
                                    input, run_stop,
                                )
                                .await;
                            });
                            cancels.register(
                                &reg_session,
                                TaskCanceller::script(handle.abort_handle(), stop),
                            );
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
                            // Snapshot before spawning (#372) — see the Rhai arm above.
                            let tools = tools.read().unwrap().clone();
                            let holly = holly.clone();
                            let skills = skills.clone();
                            let active_skill = active_skill.clone();
                            let grants = grants.clone();
                            let hooks = hooks.clone();
                            let pending = pending.clone();
                            let escape_root = escape_root.clone();
                            // Register so a `Stop` aborts this task mid-execution:
                            // aborting the future drops the exec tool's child,
                            // firing its process-group SIGKILL guard (#167/#168).
                            let reg_session = session.clone();
                            let handle = tokio::spawn(async move {
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
    active_skill: &Arc<Mutex<HashMap<SessionId, ActiveSkill>>>,
    resolver: &dyn PermissionResolver,
    chain: &[SessionId],
    grants: &dyn GrantStore,
    hooks: &Hooks,
    pending: &crate::pending::PendingDecisions,
    escape_root: Option<&EscapeRoot>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    // Resolve + apply grants first (matching the pre-seam order where `perm` was
    // computed before the hook ran), so a grant upgrade and the veto compose the
    // same way. The tool-specific argument (command/path, #173) lets an
    // argument-scoped rule resolve against the call.
    let arg = permission_arg(&tool, &input);
    let base_perm = resolve_effective(resolver, chain, &tool, &input).await;
    let perm = apply_grant(grants, &session, &tool, arg.as_deref(), base_perm);
    if let Some(reason) = hooks.run_pre_tool_use(&session, &tool, &input).await {
        seam::reply(holly, session, request_id, reason).await;
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
                session,
                request_id,
                tool,
                input,
            )
            .await;
        }
        Permission::Deny => {
            let output = format!("tool `{tool}` denied by permission profile");
            seam::reply(holly, session, request_id, output).await;
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
                rx,
                escape_grant,
                session,
                request_id,
                tool,
                input,
            )
            .await;
        }
    }
}

/// Park until the head answers the pending approval, then run-or-refuse. A
/// `Stop` (Esc-in-approval) unwinds silently: core's `wait_tool_result` sees the
/// same `Stop` on its inbox and cancels the turn, so no `ToolResult` is owed
/// (the shared park/filter is [`crate::seam::await_decision`]).
#[allow(clippy::too_many_arguments)]
async fn await_decision(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashMap<SessionId, ActiveSkill>>>,
    grants: &dyn GrantStore,
    hooks: &Hooks,
    rx: tokio::sync::oneshot::Receiver<seam::Decision>,
    escape_grant: Option<(Arc<crate::extra_roots::ExtraRootStore>, std::path::PathBuf)>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    match crate::pending::await_decision(rx).await {
        seam::Decision::Approve { scope } => {
            set_thinking(holly, &session);
            if let Some((store, abs)) = &escape_grant {
                // The prompt was forced by an out-of-root access (ADR-0109):
                // record the approval in the escape-root store so the host tool's
                // containment check lets *this tool* reach *this path*. Every scope
                // is recorded (a `Once` becomes the single-use token the tool
                // consumes); `Session`/`Always` also relax future containment and
                // let the executor skip re-asking. Per-tool by construction.
                store.record(&tool, abs, scope);
            } else if scope != ApprovalScope::Once {
                // Ordinary (in-root) approval: record the wider scopes (#174) so an
                // identical later call skips this prompt — through the pluggable
                // [`GrantStore`] (#311). `Once` records nothing.
                let arg = permission_arg(&tool, &input);
                grants.record(&session, &tool, arg.as_deref(), scope).await;
            }
            run_and_reply(
                holly,
                tools,
                skills,
                active_skill,
                hooks,
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
            seam::reply(holly, session, request_id, output).await;
        }
        // `Stop` (and a closed inbox) unwind silently; `Answer` never targets a
        // tool-approval request id.
        seam::Decision::Stop | seam::Decision::Answer { .. } => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_and_reply(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashMap<SessionId, ActiveSkill>>>,
    hooks: &Hooks,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    // `update_plan`/`update_tasks` carry no host resource (#231, ADR-0049): they
    // are not in the registry. The runtime emits their `Plan`/`TaskList` snapshot
    // — minting a **fresh** per-session seq (#157) so it takes an ordered place in
    // the content stream instead of colliding with the parked `ToolExec` seq — and
    // acks (text), instead of dispatching.
    if crate::plan_tasks::is_state_tool(&tool) {
        holly.emit_for_session(&session, |seq| {
            crate::plan_tasks::state_event(&session, seq, &tool, &input)
                .expect("is_state_tool ⇒ state_event is Some")
        });
        let ack = crate::plan_tasks::ack(&tool);
        hooks.run_post_tool_use(&session, &tool, &input, &ack).await;
        seam::reply(holly, session, request_id, ack).await;
        return;
    }
    // Every other tool executes against the host registry, returning multimodal
    // content (a text result, or an image block for `read` on an image, #221).
    // `edit`/`write` record their change into the capture scope (#202); the
    // executor mints a fresh `FileChange` seq (#157) and broadcasts the audit
    // event before replying with the `ToolResult`.
    let content = crate::file_change::capture_and_emit(
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
    let output_text = entanglement_core::content_text(&content);
    // #400, ADR-0106: a successful `load_skill` activates the session's
    // skill-scoped tool mask for the rest of this turn — parsed from the
    // result's `skill_id:` header (absent on a failed load: unknown/`user_only`
    // skill, which leaves any prior active skill untouched).
    if tool == LOAD_SKILL_TOOL {
        activate_skill(holly, skills, active_skill, &session, &output_text);
    }
    // `post_tool_use` (#199) observes the result before it is folded back — a
    // pure side-effect (formatter/telemetry); it cannot rewrite `content`.
    hooks
        .run_post_tool_use(&session, &tool, &input, &output_text)
        .await;
    seam::reply_content(holly, session, request_id, content).await;
}

/// Activate `session`'s skill mask (#400, ADR-0106) from a `load_skill` result:
/// parse its `skill_id:` header, look the skill up in the live registry for its
/// `allowed_tools`, record it, and tell any listening head via
/// [`OutEvent::SkillActive`]. A `result` with no `skill_id:` header (a failed
/// load) is a no-op — the session keeps whatever skill was active before.
fn activate_skill(
    holly: &Holly,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashMap<SessionId, ActiveSkill>>>,
    session: &SessionId,
    result: &str,
) {
    let Some(skill_id) = parse_skill_id(result) else {
        return;
    };
    let allowed_tools = skills
        .read()
        .unwrap()
        .get(skill_id)
        .and_then(|s| s.allowed_tools.clone());
    active_skill.lock().unwrap().insert(
        session.clone(),
        ActiveSkill {
            skill_id: skill_id.to_string(),
            allowed_tools: allowed_tools.clone(),
        },
    );
    holly.emit_for_session(session, |seq| OutEvent::SkillActive {
        session: session.clone(),
        seq,
        skill_id: Some(skill_id.to_string()),
        allowed_tools,
    });
}

/// Clear `session`'s active skill mask (#400, ADR-0106) — the turn's `Done`, the
/// natural end of a skill's scope. A no-op (no wire event) when no skill was
/// active, matching [`activate_skill`]'s "only tell a head about a real change"
/// shape.
fn clear_active_skill(
    holly: &Holly,
    active_skill: &Arc<Mutex<HashMap<SessionId, ActiveSkill>>>,
    session: &SessionId,
) {
    if active_skill.lock().unwrap().remove(session).is_some() {
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
    use crate::tool_names::UPDATE_PLAN_TOOL;

    #[test]
    fn classify_maps_each_orchestration_tool_to_its_route() {
        assert_eq!(Intercept::classify(AGENT_TOOL), Intercept::Spawn);
        assert_eq!(Intercept::classify(AGENT_SPAWN_TOOL), Intercept::Spawn);
        assert_eq!(Intercept::classify(AGENT_POLL_TOOL), Intercept::AgentPoll);
        assert_eq!(Intercept::classify(ASK_USER_TOOL), Intercept::AskUser);
        assert_eq!(
            Intercept::classify(PROPOSE_PLAN_TOOL),
            Intercept::ProposePlan
        );
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
            UPDATE_PLAN_TOOL,
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
        assert!(Intercept::AgentPoll.bypasses_permission());
        assert!(Intercept::AskUser.bypasses_permission());
        assert!(Intercept::ProposePlan.bypasses_permission());
        assert!(!Intercept::Rhai.bypasses_permission());
        assert!(!Intercept::Permission.bypasses_permission());
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
