//! The orchestration handlers of the interception ladder (issue #451,
//! split out of `ladder.rs`): `agent`/`agent_send`/`poll`/`ask_user`/
//! `propose_plan` and the discovery lookups. The bookkeeping and discovery
//! routes touch no permission-graded host resource and bypass grading
//! (ADR-0207 §3's `Capability::Control`; `propose_plan` grades its own
//! `plan` class). The spawning pair climbs the dispatch ladder first
//! ([`gate`], ADR-0232) — a spawn starts a session that runs tools.

use std::collections::HashMap;

use entanglement_core::{SessionId, ToolOverlayEntry};

use crate::cancel::TaskCanceller;
use crate::discover;
use crate::permission::{ancestor_chain, spawn_refusal};
use crate::seam;
use crate::subagent::SpawnGuard;
use crate::tool_names::{
    AGENT_SEND_TOOL, AGENT_TOOL, LIST_AGENTS_TOOL, LIST_MCP_TOOL, LIST_MODELS_TOOL, MCP_TOOLS_TOOL,
};
use crate::tool_runner::dispatch::{Call, Dispatcher};

use super::LadderCtx;

/// Climb the dispatch ladder for a spawning call (ADR-0232): `true` when it
/// cleared; on a refusal the model gets it as an error result, and a `Stop`
/// during an approval park owes nothing (core cancels the turn).
async fn gate(
    dispatcher: Dispatcher,
    tool: &str,
    session: &SessionId,
    request_id: &str,
    input: &str,
) -> bool {
    let call = Call {
        request_id: request_id.to_string(),
        tool: tool.to_string(),
        input: input.to_string(),
        envelope: None,
        rhai_path: None,
    };
    match dispatcher.gate(&call).await {
        Some(Ok(())) => true,
        Some(Err(why)) => {
            let holly = dispatcher.holly.clone();
            seam::reply(&holly, session.clone(), request_id.to_string(), why, true).await;
            false
        }
        None => false,
    }
}

pub(super) async fn spawn(
    ctx: &LadderCtx,
    spawn_guard: &mut SpawnGuard,
    overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // Spawn control: an unknown target name or model refuses here, and the
    // session's mode bounds the tree (`max_depth`/`max_agents`,
    // `SpawnGuard::try_spawn`, ADR-0207 §6). Past those, the call climbs the
    // dispatch ladder like any tool (ADR-0232) inside the detached task — an
    // approval park must not hold the loop. The slot is reserved first (the
    // guard is loop state) and released if the ladder refuses.
    // Subscribe (after the gate, inside the task) *before* the child is
    // spawned so its `Done` can't race ahead of the watcher.
    let dispatcher = super::graded::dispatcher(ctx, spawn_guard, overlays, &session);
    let blocking = !crate::subagent::is_background(&input);
    let target = crate::subagent::target_agent(&input);
    // The child's optional model override (#560 P12, ADR-0207 §12): resolved
    // + validated against the catalog *before* any child is minted, exactly
    // like the agent-target check below — an unknown id refuses the whole
    // spawn rather than silently falling back to inherit.
    let model_request = crate::subagent::target_model(&input);
    let model_result =
        crate::permission::resolve_model(model_request.as_deref(), ctx.catalog.as_deref());
    let refusal = {
        let profiles = ctx
            .agents
            .read()
            .expect("agent-profile registry lock poisoned");
        spawn_refusal(&target, &profiles)
    }
    .or_else(|| model_result.as_ref().err().cloned());
    // The session's own mode bounds its spawn (ADR-0207
    // §6: mode applies to the whole spawn sub-tree, so
    // there is no per-spawn override to consult) — an
    // unseen session or unresolvable mode name fails
    // closed, mirroring `ModeResolver`'s own
    // fail-closed default.
    let mode = refusal.is_none().then(|| {
        let mode_name = ctx
            .perm_modes
            .lock()
            .expect("permission-mode mutex poisoned")
            .get(&session)
            .cloned();
        mode_name.and_then(|name| ctx.mode_table.get(&name).cloned())
    });
    let spawn_result = match (refusal, mode) {
        (Some(refusal), _) => Err(refusal),
        (None, Some(Some(mode))) => spawn_guard.try_spawn(&session, &mode),
        (None, _) => Err("sub-agent spawn refused: session has no resolved \
             permission mode."
            .to_string()),
    };
    match spawn_result {
        Ok(slot) => {
            // `refusal` was `None` here, so `model_result` is `Ok` too — the
            // two checks above refuse together (`refusal`'s `or_else` folds
            // a model error in), never independently.
            let model_pin = model_result.ok().flatten();
            let registry = ctx.registry.clone();
            let retained = ctx.retained.clone();
            let holly = ctx.holly.clone();
            tokio::spawn(async move {
                // Held until the child's answer is in (both launch shapes
                // watch the child to the end), so the mode's `max_agents`
                // counts running sub-agents, not spawns ever made.
                let _slot = slot;
                if !gate(dispatcher, AGENT_TOOL, &session, &request_id, &input).await {
                    return;
                }
                // Subscribed only now: the gate may park on an approval card
                // for as long as the user takes, and a receiver held across
                // that would lag past the outbox capacity and lose the
                // child's answer. Still before the child's `Spawn` is sent.
                let child_events = holly.subscribe();
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
                        model_pin,
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
                        model_pin,
                    )
                    .await;
                }
            });
        }
        // Refused: no child minted, but still answer the
        // parent's parked tool call so its turn
        // continues with a clear explanation.
        Err(refusal) => {
            let holly = ctx.holly.clone();
            tokio::spawn(async move {
                seam::reply(&holly, session, request_id, refusal, true).await;
            });
        }
    }
}

pub(super) async fn agent_send(
    ctx: &LadderCtx,
    spawn_guard: &SpawnGuard,
    overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // No spawn-budget/depth gate here (#609): this is a *reply* into an
    // already-authorized child, not a new spawn. The dispatch ladder grades
    // it like any tool (ADR-0232), then `AgentRegistry::begin_send` (run
    // inside the task) checks ownership (only the launching session may
    // send) and lifecycle (ADR-0162 §4). Subscribe after the gate (an
    // approval park must not lag the receiver) but before the follow-up is
    // sent, mirroring `Spawn`, so the child's events can't race ahead of the
    // watcher.
    let dispatcher = super::graded::dispatcher(ctx, spawn_guard, overlays, &session);
    let registry = ctx.registry.clone();
    let retained = ctx.retained.clone();
    let holly = ctx.holly.clone();
    tokio::spawn(async move {
        if !gate(dispatcher, AGENT_SEND_TOOL, &session, &request_id, &input).await {
            return;
        }
        let child_events = holly.subscribe();
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

pub(super) async fn poll(ctx: &LadderCtx, session: SessionId, request_id: String, input: String) {
    let regs = crate::poll::PollRegistries {
        jobs: ctx.jobs.clone(),
        agents: ctx.registry.clone(),
        retained: ctx.retained.clone(),
        scripts: ctx.scripts.clone(),
        pending: ctx.pending.clone(),
        questions: ctx.open_questions.clone(),
    };
    let holly = ctx.holly.clone();
    tokio::spawn(async move {
        crate::poll::run_poll(holly, regs, session, request_id, input).await;
    });
}

pub(super) async fn ask_user(
    ctx: &LadderCtx,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // Registers with `pending` (and `open_questions`,
    // #515) before emitting the question (#156), so a
    // fast answer routes to the parked waiter rather
    // than racing a per-task broadcast park.
    let pending = ctx.pending.clone();
    let open_questions = ctx.open_questions.clone();
    let holly = ctx.holly.clone();
    // The session's mode bounds how long a question waits
    // for an answer (ADR-0207 §11, stage 5c) — resolved the
    // same way `graded::permission` resolves `call_mode`,
    // just for `Limits` instead of a grade.
    let timeout = mode_limits(ctx, &session).and_then(|l| crate::run_limits::timeout(&l));
    tokio::spawn(async move {
        crate::ask_user::run_ask_user(
            holly,
            pending,
            open_questions,
            session,
            request_id,
            input,
            timeout,
        )
        .await;
    });
}

/// Resolve `session`'s current [`crate::mode::Limits`] against `ctx`'s mode
/// table (`None` for an unseen session or an unresolvable mode name — the
/// same fail-closed shape `ModeResolver` already uses elsewhere, though
/// here it just means "no timeout", never a widened grade).
fn mode_limits(ctx: &LadderCtx, session: &SessionId) -> Option<crate::mode::Limits> {
    let name = ctx
        .perm_modes
        .lock()
        .expect("permission-mode mutex poisoned")
        .get(session)
        .cloned()?;
    ctx.mode_table.get(&name).map(|m| m.limits)
}

pub(super) async fn propose_plan(
    ctx: &LadderCtx,
    spawn_guard: &SpawnGuard,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // ADR-0207 §7: plan authorship is graded by the
    // session's mode (`Capability::Plan`), resolved
    // inside the detached task exactly like the
    // generic `Permission` route below
    // (`resolve_effective` over the same ancestor
    // chain) — a pluggable resolver can hit a DB, so
    // it never runs in this loop. Approval past that
    // grade is still the tool's own unconditional
    // semantics; no sponsored spawn, no SpawnGuard
    // mutation needed here any more.
    let chain = ancestor_chain(spawn_guard, &session);
    let resolver = ctx.resolver.clone();
    let call_mode = ctx
        .perm_modes
        .lock()
        .expect("permission-mode mutex poisoned")
        .get(&session)
        .cloned()
        .unwrap_or_default();
    let pending = ctx.pending.clone();
    let holly = ctx.holly.clone();
    let plan_files = ctx.plan_files.clone();
    let plan_root = ctx.plan_root.clone();
    // Registered with `CancelRegistry` (#513): a
    // `Stop` targeting the plan session aborts the
    // Ask-wait at any point, with no reply owed
    // (core cancels the turn on the same `Stop`).
    let reg_session = session.clone();
    let handle = tokio::spawn(async move {
        crate::propose_plan::run_propose_plan(
            holly, pending, resolver, chain, call_mode, plan_files, plan_root, session, request_id,
            input,
        )
        .await;
    });
    ctx.cancels
        .register(&reg_session, TaskCanceller::task(handle.abort_handle()));
}

pub(super) async fn discover(
    ctx: &LadderCtx,
    tool: String,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // Read-only, never graded (ADR-0226): a snapshot read and a reply. The
    // static rosters render here in the loop; the MCP pair runs detached since
    // `mcp_tools` may connect a server.
    let holly = ctx.holly.clone();
    let static_reply = match tool.as_str() {
        LIST_AGENTS_TOOL => Some(discover::agents_list(
            &ctx.agents
                .read()
                .expect("agent-profile registry lock poisoned"),
        )),
        LIST_MODELS_TOOL => Some(discover::models_list(ctx.catalog.as_deref())),
        LIST_MCP_TOOL | MCP_TOOLS_TOOL => None,
        _ => Some(discover::skills_list(
            &ctx.skills
                .read()
                .expect("skill registry lock poisoned")
                .disclosures(),
        )),
    };
    if let Some(output) = static_reply {
        tokio::spawn(async move {
            seam::reply(&holly, session, request_id, output, false).await;
        });
        return;
    }
    let (avail, active, registry) = (
        ctx.mcp_avail.clone(),
        ctx.mcp_active.clone(),
        ctx.tools.clone(),
    );
    let (scopes, http, advertising) = (
        ctx.mcp_scopes.clone(),
        ctx.mcp_http.clone(),
        ctx.advertising.clone(),
    );
    // Registered so a `Stop` aborts a slow first connect mid-flight.
    let reg_session = session.clone();
    let handle = tokio::spawn(async move {
        let src = discover::McpSources {
            avail: &avail,
            active: &active,
            registry: &registry,
            scopes: scopes.as_deref(),
            http: http.as_ref(),
            delivered: &advertising.delivered,
        };
        let (output, is_error) = if tool == LIST_MCP_TOOL {
            (discover::list_mcp(&src, &session), false)
        } else {
            discover::mcp_tools(&src, &session, &input).await
        };
        seam::reply(&holly, session, request_id, output, is_error).await;
    });
    ctx.cancels
        .register(&reg_session, TaskCanceller::task(handle.abort_handle()));
}
