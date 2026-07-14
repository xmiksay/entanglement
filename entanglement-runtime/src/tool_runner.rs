//! Runtime tool executor. Owns everything about a tool call that is *not* the
//! engine's business: the `Allow | Ask | Deny` permission decision (#59), the
//! approval UX round-trip, and the actual execution against the host-tool
//! [`ToolRegistry`] (#58, ADR-0006/0010).
//!
//! Core emits [`OutEvent::ToolExec`] for **every** host tool and parks on
//! [`InMsg::ToolResult`]; it no longer consults `PermissionProfile`. This task:
//!
//! 1. tracks each session's active [`AgentProfile`] from `SessionStarted` /
//!    `AgentChanged` (ADR-0020), resolved against the [`ProfileRegistry`] it was
//!    handed at startup;
//! 2. on `ToolExec`, resolves the permission for the tool:
//!    - `Deny` → replies `ToolResult("…denied…")` without running it;
//!    - `Allow` → runs it and replies `ToolResult`;
//!    - `Ask` → emits [`OutEvent::ToolRequest`] (the approval prompt) and awaits
//!      the head's `Approve`/`Reject`/`Stop` on the engine's inbound fan-out
//!      ([`Holly::subscribe_inbound`]), then runs-or-refuses accordingly.
//!
//! Each request runs on its own detached task so a slow tool (or a pending
//! approval) can't stall anything else. Core dispatches a model turn's tool
//! calls as a **batch** (#270, ADR-0061): every `ToolExec` of the batch is
//! emitted up front and the turn parks until all results have returned, so
//! multiple executor tasks — and multiple pending approvals — per session are
//! normal. `seam::await_decision` filters by `(session, request_id)`, which
//! keeps concurrently parked approvals from stealing each other's answers.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use entanglement_core::{
    AgentProfile, AgentState, ApprovalScope, Holly, InMsg, OutEvent, Permission, PermissionProfile,
    ProfileRegistry, SessionId, ToolCall,
};

use crate::tools::ToolRegistry;
use tokio::sync::broadcast::error::RecvError;

use crate::cancel::{CancelRegistry, TaskCanceller};
use crate::grants::GrantStore;
use crate::hooks::Hooks;
use crate::permission::{
    clamp_to_base, effective_permission, permission_arg, spawn_refusal, tool_masked,
};
use crate::seam;
use crate::tool_names::{
    AGENT_POLL_TOOL, AGENT_SPAWN_TOOL, AGENT_TOOL, ASK_USER_TOOL, PROPOSE_PLAN_TOOL, RHAI_TOOL,
};

/// Upgrade a resolved `Ask` to `Allow` when `(session, tool, arg)` is already
/// granted (#174): a session-scoped or persisted "always allow" grant lets an
/// *identical* later call skip the prompt. Only `Ask` is widened — a `Deny` (a
/// hard policy floor) and an outright `Allow` pass through untouched.
fn apply_grant(
    grants: &Mutex<GrantStore>,
    session: &SessionId,
    tool: &str,
    arg: Option<&str>,
    perm: Permission,
) -> Permission {
    if perm == Permission::Ask && grants.lock().unwrap().is_granted(session, tool, arg) {
        Permission::Allow
    } else {
        perm
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
    let hooks = Arc::new(hooks);
    let mut sub = holly.subscribe();
    // Subscribe to the inbound fan-out *synchronously*, before this function
    // returns, so a `Prompt`/`Stop` the caller sends right after spawning can't
    // race ahead of the watcher's subscription (the `user_prompt_submit` hook,
    // #199, depends on catching that first prompt).
    let inbound = holly.subscribe_inbound();
    let holly = holly.clone();
    tokio::spawn(async move {
        // Active profile per session, folded from lifecycle events in the order
        // the engine emits them (a session's `AgentChanged` always precedes any
        // `ToolExec` it produces under that profile).
        let mut active: HashMap<SessionId, AgentProfile> = HashMap::new();
        // Bounds the spawn tree (#76): tracks parent links from lifecycle events
        // and per-root spawn budgets. Lives in this single-threaded loop, so the
        // spawn decision below is race-free.
        let mut spawn_guard = crate::subagent::SpawnGuard::new();
        // Answer + timing per launched sub-agent, keyed by its handle (#89).
        // Shared with the detached launch watchers and `agent_poll` tasks.
        let registry = crate::agent_poll::AgentRegistry::default();
        // "Always allow" grants (#174): the persisted set loaded from the managed
        // file, plus per-session grants recorded live. Shared with the per-request
        // dispatch tasks, which record the wider scopes off an `Approve`; the loop
        // reads it to skip the prompt for an already-granted call. `Mutex` (never
        // held across an `.await`) keeps the loop's reads ordered with those writes.
        let grants = Arc::new(Mutex::new(GrantStore::load()));
        // In-flight tool tasks per session (#167). A `Stop` on the inbound
        // fan-out aborts every task registered for that session, so a running
        // `bash`/`call` command or `rhai` script is actually cancelled — core
        // only clears the parked turn state, it never owns the execution.
        let cancels = CancelRegistry::default();
        {
            // Watch inbound for `Stop` in its own task so the outbound loop below
            // is untouched; both share the same `cancels` registry. The same task
            // fires the `user_prompt_submit` hooks (#199) off each `Prompt` — the
            // engine's inbound fan-out is the runtime-side ingress seam.
            let cancels = cancels.clone();
            let hooks = hooks.clone();
            let mut inbound = inbound;
            tokio::spawn(async move {
                loop {
                    match inbound.recv().await {
                        Ok(InMsg::Stop { session }) => cancels.cancel_session(&session),
                        Ok(InMsg::Prompt { session, content })
                            if !hooks.user_prompt_submit.is_empty() =>
                        {
                            // Detach so a slow hook can't stall the Stop watcher.
                            let hooks = hooks.clone();
                            tokio::spawn(async move {
                                let text = entanglement_core::content_text(&content);
                                hooks.run_user_prompt_submit(&session, &text).await;
                            });
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {}
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
                    if let Some(p) = profiles.get(&profile) {
                        active.insert(session, p.clone());
                    }
                }
                Ok(OutEvent::AgentChanged { session, agent, .. }) => {
                    if let Some(p) = profiles.get(&agent) {
                        active.insert(session, p.clone());
                    }
                }
                Ok(OutEvent::SessionEnded { session, .. }) => {
                    // Drop the closed session's in-memory grants (#174); persisted
                    // "always" grants survive.
                    grants.lock().unwrap().forget_session(&session);
                    // Its in-flight tool bookkeeping is moot once the session ends.
                    cancels.forget_session(&session);
                }
                Ok(OutEvent::ToolExec {
                    session,
                    seq,
                    request_id,
                    tool,
                    input,
                    ..
                }) => {
                    // Physical tool restriction (#116, ADR-0038): a tool outside
                    // the session's effective advertised set — its profile's
                    // allowlist/denylist, intersected down the ancestor chain —
                    // does not exist for this agent. Refuse before any other
                    // handling (spawn interception, permission), so even a
                    // hallucinated call to a masked `edit`/`agent_spawn` is a
                    // hard boundary, not a persona nudge. Core already withholds
                    // the schema; this closes the gap if the model calls it anyway.
                    if tool_masked(&active, &spawn_guard, &session, &tool) {
                        let holly = holly.clone();
                        tokio::spawn(async move {
                            let output =
                                format!("tool `{tool}` is not available to this agent (restricted by profile)");
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
                            if let Some(refusal) =
                                spawn_refusal(active.get(&session), &target, &profiles)
                            {
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
                            // Subscribe *before* handing off so a fast answer can't
                            // race ahead of the parked executor task.
                            let inbound = holly.subscribe_inbound();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::ask_user::run_ask_user(
                                    holly, inbound, session, seq, request_id, input,
                                )
                                .await;
                            });
                        }
                        Intercept::ProposePlan => {
                            // Approve just acks the model (no engine plan state,
                            // #231); the head handles the fresh-`build`-session
                            // handoff (head policy, no new protocol surface).
                            let inbound = holly.subscribe_inbound();
                            let holly = holly.clone();
                            tokio::spawn(async move {
                                crate::propose_plan::run_propose_plan(
                                    holly, inbound, session, seq, request_id, input,
                                )
                                .await;
                            });
                        }
                        Intercept::Rhai => {
                            // The bindings resolve permission live against this
                            // loop's profile state — captured here as a per-run
                            // snapshot and moved into the script task. The tool's
                            // *own* Allow/Ask/Deny is resolved the same way.
                            let arg = permission_arg(&tool, &input);
                            let self_perm = apply_grant(
                                &grants,
                                &session,
                                &tool,
                                arg.as_deref(),
                                clamp_to_base(
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
                                ),
                            );
                            let policy = crate::script::BindingPolicy::capture(
                                &active,
                                &spawn_guard,
                                &session,
                                &base,
                            );
                            let inbound = holly.subscribe_inbound();
                            let tools = tools.clone();
                            let holly = holly.clone();
                            // The blocking engine can't be aborted, so pair the
                            // task abort with a cooperative stop flag its progress
                            // callback polls (#167).
                            let stop = Arc::new(AtomicBool::new(false));
                            let reg_session = session.clone();
                            let run_stop = stop.clone();
                            let handle = tokio::spawn(async move {
                                crate::script::run_rhai(
                                    holly, tools, policy, self_perm, session, seq, request_id,
                                    inbound, input, run_stop,
                                )
                                .await;
                            });
                            cancels.register(
                                &reg_session,
                                TaskCanceller::script(handle.abort_handle(), stop),
                            );
                        }
                        Intercept::Permission => {
                            // Resolve permission before spawning so the read of
                            // `active` stays ordered with the lifecycle events
                            // above. A child sub-agent is clamped to its parent
                            // chain (#77): its effective permission can never exceed
                            // any ancestor's, so a child cannot touch the shared tree
                            // in ways the parent couldn't. A root session (no
                            // ancestors) resolves to its own profile unchanged; a
                            // session we never saw start defaults to `Allow`. The
                            // tool-specific argument (command/path, #173) lets an
                            // argument-scoped rule resolve against the actual call.
                            let arg = permission_arg(&tool, &input);
                            let perm = apply_grant(
                                &grants,
                                &session,
                                &tool,
                                arg.as_deref(),
                                clamp_to_base(
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
                                ),
                            );
                            let tools = tools.clone();
                            let holly = holly.clone();
                            let grants = grants.clone();
                            let hooks = hooks.clone();
                            // Register so a `Stop` aborts this task mid-execution:
                            // aborting the future drops the exec tool's child,
                            // firing its process-group SIGKILL guard (#167/#168).
                            let reg_session = session.clone();
                            let handle = tokio::spawn(async move {
                                dispatch(
                                    &holly, &tools, &grants, &hooks, session, seq, request_id,
                                    tool, input, perm,
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
/// A `pre_tool_use` hook (#199) runs first and can **veto** the call before the
/// permission decision: a non-zero-exit hook short-circuits with a denial
/// `ToolResult`, so the tool neither prompts nor runs. Cleared hooks fall through
/// to the normal `Allow | Ask | Deny` dispatch.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    holly: &Holly,
    tools: &ToolRegistry,
    grants: &Mutex<GrantStore>,
    hooks: &Hooks,
    session: SessionId,
    seq: u64,
    request_id: String,
    tool: String,
    input: String,
    perm: Permission,
) {
    if let Some(reason) = hooks.run_pre_tool_use(&session, &tool, &input).await {
        seam::reply(holly, session, request_id, reason).await;
        return;
    }
    match perm {
        Permission::Allow => {
            run_and_reply(holly, tools, hooks, session, seq, request_id, tool, input).await;
        }
        Permission::Deny => {
            let output = format!("tool `{tool}` denied by permission profile");
            seam::reply(holly, session, request_id, output).await;
        }
        Permission::Ask => {
            // Subscribe *before* prompting so a fast approval can't race ahead of
            // us. The prompt reuses the `ToolExec` seq: core's next content event
            // (the `ToolOutput`) carries a higher seq, so a head's monotonic
            // dedupe still honors the request.
            let mut inbound = holly.subscribe_inbound();
            let _ = holly.events().send(OutEvent::ToolRequest {
                session: session.clone(),
                seq,
                request_id: request_id.clone(),
                tool: tool.clone(),
                input: input.clone(),
            });
            let _ = holly.events().send(OutEvent::Status {
                session: session.clone(),
                state: AgentState::WaitingApproval,
            });
            await_decision(
                holly,
                tools,
                grants,
                hooks,
                &mut inbound,
                session,
                seq,
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
    grants: &Mutex<GrantStore>,
    hooks: &Hooks,
    inbound: &mut tokio::sync::broadcast::Receiver<InMsg>,
    session: SessionId,
    seq: u64,
    request_id: String,
    tool: String,
    input: String,
) {
    match seam::await_decision(inbound, &session, &request_id).await {
        seam::Decision::Approve { scope } => {
            set_thinking(holly, &session);
            // Record the wider scopes (#174) so an identical later call skips
            // this prompt. `Once` records nothing. Done before the (awaiting)
            // run so the guard is dropped before the `.await`.
            if scope != ApprovalScope::Once {
                let arg = permission_arg(&tool, &input);
                grants
                    .lock()
                    .unwrap()
                    .record(&session, &tool, arg.as_deref(), scope);
            }
            run_and_reply(holly, tools, hooks, session, seq, request_id, tool, input).await;
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
    hooks: &Hooks,
    session: SessionId,
    seq: u64,
    request_id: String,
    tool: String,
    input: String,
) {
    // `update_plan`/`update_tasks` carry no host resource (#231, ADR-0049): they
    // are not in the registry. The runtime emits their `Plan`/`TaskList` snapshot
    // — reusing the `ToolExec` seq — and acks (text), instead of dispatching.
    if crate::plan_tasks::is_state_tool(&tool) {
        if let Some(ev) = crate::plan_tasks::state_event(&session, seq, &tool, &input) {
            let _ = holly.events().send(ev);
        }
        let ack = crate::plan_tasks::ack(&tool);
        hooks.run_post_tool_use(&session, &tool, &input, &ack).await;
        seam::reply(holly, session, request_id, ack).await;
        return;
    }
    // Every other tool executes against the host registry, returning multimodal
    // content (a text result, or an image block for `read` on an image, #221).
    // `edit`/`write` record their change into the capture scope (#202); the
    // executor stamps it with this call's session/seq and broadcasts the
    // `FileChange` audit event before replying with the `ToolResult`.
    let content = crate::file_change::capture_and_emit(
        holly.events(),
        &session,
        seq,
        tools.execute(&ToolCall {
            id: request_id.clone(),
            name: tool.clone(),
            input: input.clone(),
        }),
    )
    .await;
    // `post_tool_use` (#199) observes the result before it is folded back — a
    // pure side-effect (formatter/telemetry); it cannot rewrite `content`.
    hooks
        .run_post_tool_use(
            &session,
            &tool,
            &input,
            &entanglement_core::content_text(&content),
        )
        .await;
    seam::reply_content(holly, session, request_id, content).await;
}

fn set_thinking(holly: &Holly, session: &SessionId) {
    let _ = holly.events().send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::Thinking,
    });
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
}
