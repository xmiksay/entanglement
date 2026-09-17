//! The orchestration handlers of the interception ladder (issue #451,
//! split out of `ladder.rs`): `agent`/`agent_send`/`poll`/`ask_user`/
//! `propose_plan`/`explore`+`describe` all touch no permission-graded host
//! resource (ADR-0207 §3's `Capability::Control`, or a runtime-owned prompt/
//! finalize tool with its own unconditional semantics) — [`Intercept::
//! bypasses_permission`][super::Intercept::bypasses_permission] is true for
//! every route handled here.

use entanglement_core::SessionId;

use crate::cancel::TaskCanceller;
use crate::discover;
use crate::permission::{ancestor_chain, spawn_refusal};
use crate::seam;
use crate::subagent::SpawnGuard;
use crate::tool_names::{DESCRIBE_TOOL, EXPLORE_TOOL, RESPONSES_TOOL_SEARCH_TOOL};

use super::LadderCtx;

pub(super) async fn spawn(
    ctx: &LadderCtx,
    spawn_guard: &mut SpawnGuard,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // Spawn control (ADR-0207 §6): the only refusal left
    // is an unknown target name — spawning itself is
    // never graded and any registered agent is a valid
    // target. Bounded instead by the session's mode
    // `max_depth`/`max_agents` (`SpawnGuard::try_spawn`).
    // Subscribe *before* handing off so the child's
    // `Done` can't race ahead of the watcher.
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
        Ok(()) => {
            // `refusal` was `None` here, so `model_result` is `Ok` too — the
            // two checks above refuse together (`refusal`'s `or_else` folds
            // a model error in), never independently.
            let model_pin = model_result.ok().flatten();
            let child_events = ctx.holly.subscribe();
            let registry = ctx.registry.clone();
            let retained = ctx.retained.clone();
            let holly = ctx.holly.clone();
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
    session: SessionId,
    request_id: String,
    input: String,
) {
    // No spawn-budget/depth gate here (#609): this is a
    // *reply* into an already-authorized child, not a
    // new spawn — `AgentRegistry::begin_send` (run
    // inside the task) is the whole gate: ownership
    // (only the launching session may send) plus the
    // lifecycle check (ADR-0162 §4). Subscribe *before*
    // handing off, mirroring `Spawn`, so the child's
    // events can't race ahead of the watcher.
    let child_events = ctx.holly.subscribe();
    let registry = ctx.registry.clone();
    let retained = ctx.retained.clone();
    let holly = ctx.holly.clone();
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

pub(super) async fn poll(ctx: &LadderCtx, session: SessionId, request_id: String, input: String) {
    let registry = ctx.registry.clone();
    let jobs = ctx.jobs.clone();
    let retained = ctx.retained.clone();
    let scripts = ctx.scripts.clone();
    let holly = ctx.holly.clone();
    tokio::spawn(async move {
        crate::poll::run_poll(
            holly, jobs, registry, retained, scripts, session, request_id, input,
        )
        .await;
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
    // Read-only, non-maskable, always-`Allow` (#560,
    // ADR-0196 §4) — no permission check, no approval
    // round-trip, just a snapshot read and a reply.
    let registry_snapshot = ctx
        .tools
        .read()
        .expect("tool registry lock poisoned")
        .clone();
    let skills_snapshot = ctx
        .skills
        .read()
        .expect("skill registry lock poisoned")
        .clone();
    let mcp_avail = ctx.mcp_avail.clone();
    let mcp_active = ctx.mcp_active.clone();
    let mcp_scopes = ctx.mcp_scopes.clone();
    let advertising = ctx.advertising.clone();
    let holly = ctx.holly.clone();
    // `explore(kind: "pending")` (#560 P12, ADR-0207 §12) forks here, before
    // either the tool-index path or `KindsCtx` build below: it needs the
    // engine-wide job/script/retained/approval/question registries this
    // ladder already holds, which neither `run_explore`'s nor
    // `KindsCtx`'s shape carries — those cover only the *static* non-tool
    // kinds (agents/skills/models/modes).
    if tool == EXPLORE_TOOL && discover::peek_kind(&input).as_deref() == Some("pending") {
        let src_agents = ctx.registry.clone();
        let src_jobs = ctx.jobs.clone();
        let src_scripts = ctx.scripts.clone();
        let src_retained = ctx.retained.clone();
        let src_pending = ctx.pending.clone();
        let src_questions = ctx.open_questions.clone();
        tokio::spawn(async move {
            let src = discover::PendingSources {
                agents: &src_agents,
                jobs: &src_jobs,
                scripts: &src_scripts,
                retained: &src_retained,
                pending: &src_pending,
                questions: &src_questions,
            };
            let output = discover::build_pending_report(&src, &session);
            seam::reply(&holly, session, request_id, output, false).await;
        });
        return;
    }
    let kinds_ctx = discover::KindsCtx {
        agents: ctx
            .agents
            .read()
            .expect("agent-profile registry lock poisoned")
            .clone(),
        skills: skills_snapshot.clone(),
        catalog: ctx.catalog.clone(),
        modes: ctx.mode_table.clone(),
    };
    if tool == EXPLORE_TOOL {
        tokio::spawn(async move {
            discover::run_explore(
                &holly,
                &registry_snapshot,
                &mcp_avail,
                &mcp_active,
                skills_snapshot.as_ref(),
                &kinds_ctx,
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
                Some(&kinds_ctx),
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
