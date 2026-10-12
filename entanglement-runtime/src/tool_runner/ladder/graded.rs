//! The permission-graded handlers of the interception ladder (issue #451):
//! `rhai` (behind the `rhai` feature) and the generic `Permission` route —
//! the two handlers [`Intercept::bypasses_permission`][super::Intercept::bypasses_permission]
//! is `false` for. Both build one [`Dispatcher`] from the executor's shared
//! state: the generic route dispatches its call through it, and `rhai` hands
//! it to the script so every binding climbs the same ladder (ADR-0227).

use std::collections::HashMap;
#[cfg(feature = "rhai")]
use std::sync::atomic::AtomicBool;
#[cfg(feature = "rhai")]
use std::sync::Arc;

use entanglement_core::{SessionId, ToolEnvelope, ToolOverlayEntry};

use crate::cancel::TaskCanceller;
use crate::permission::ancestor_chain;
#[cfg(feature = "rhai")]
use crate::permission_path::grading_arg;
use crate::subagent::SpawnGuard;
use crate::tool_runner::dispatch::{Call, Dispatcher};

use super::LadderCtx;

/// Snapshot everything a call needs before spawning, so it stays ordered
/// with the lifecycle events the loop already folded: the ancestor chain
/// (ADR-0024), the chain's overlays (#539/#628/#634), the session's mode and
/// its limits (ADR-0207 §8/§11), and the registry (#372 — a brief read lock,
/// never held across an `.await`). An unseen session's empty mode name never
/// matches a grant, and its resolver grade fails closed (#156).
fn dispatcher(
    ctx: &LadderCtx,
    spawn_guard: &SpawnGuard,
    overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
    session: &SessionId,
) -> Dispatcher {
    let chain = ancestor_chain(spawn_guard, session);
    let overlays = chain
        .iter()
        .filter_map(|s| overlays.get(s).map(|o| (s.clone(), o.clone())))
        .collect();
    let mode = ctx
        .perm_modes
        .lock()
        .expect("permission-mode mutex poisoned")
        .get(session)
        .cloned()
        .unwrap_or_default();
    // An unresolvable mode falls back to `Limits::default()` — wait forever,
    // never an unattended posture nobody configured.
    let limits = ctx
        .mode_table
        .get(&mode)
        .map(|m| m.limits)
        .unwrap_or_default();
    Dispatcher {
        holly: ctx.holly.clone(),
        tools: ctx
            .tools
            .read()
            .expect("tool registry lock poisoned")
            .clone(),
        registry: ctx.tools.clone(),
        skills: ctx.skills.clone(),
        active_skill: ctx.active_skill.clone(),
        resolver: ctx.resolver.clone(),
        chain,
        grants: ctx.grants.clone(),
        hooks: ctx.hooks.clone(),
        pending: ctx.pending.clone(),
        escape_root: ctx.escape_root.clone(),
        overlays,
        ceiling: ctx.base.clone(),
        advertising: ctx.advertising.clone(),
        validation: ctx.validation.clone(),
        mcp_avail: ctx.mcp_avail.clone(),
        mcp_active: ctx.mcp_active.clone(),
        mcp_http: ctx.mcp_http.clone(),
        mcp_scopes: ctx.mcp_scopes.clone(),
        session: session.clone(),
        mode,
        mode_table: ctx.mode_table.clone(),
        limits,
        denials: ctx.denials.clone(),
        http_gate: ctx.http_gate.clone(),
    }
}

#[cfg(feature = "rhai")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn rhai(
    ctx: &LadderCtx,
    spawn_guard: &SpawnGuard,
    overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
    tool: String,
    session: SessionId,
    seq: u64,
    request_id: String,
    input: String,
) {
    // `rhai`'s own grade resolves through the same resolver + chain clamp as
    // a direct call (ADR-0207 stage 4b), inside the detached task since a
    // pluggable resolver may hit a DB. Root-relative (#485, ADR-0125).
    let arg = grading_arg(
        &tool,
        &input,
        ctx.escape_root.as_ref().map(|er| er.root.as_path()),
    );
    let dispatcher = dispatcher(ctx, spawn_guard, overlays, &session);
    let launch = crate::script::Launch {
        skills: ctx
            .skills
            .read()
            .expect("skill registry lock poisoned")
            .clone(),
        workdir: ctx
            .workdirs
            .get(&session)
            .map(|w| (ctx.plan_root.clone(), w)),
        scripts: ctx.scripts.clone(),
    };
    // The blocking engine can't be aborted, so the task abort pairs with a
    // cooperative stop flag its progress callback polls (#167).
    let stop = Arc::new(AtomicBool::new(false));
    let reg_session = session.clone();
    let run_stop = stop.clone();
    // A `background: true` script (#637, ADR-0185) survives a session `Stop`
    // like a background `bash`/`call` job: never registered with the
    // canceller; `poll`'s `kill: true` trips the same flag.
    let background = crate::script::is_background(&input);
    let cancels = ctx.cancels.clone();
    let handle = tokio::spawn(crate::file_backup::with_exec_seq(seq, async move {
        let self_perm = crate::tool_runner::apply_grant(
            &*dispatcher.grants,
            &session,
            &tool,
            arg.as_deref(),
            crate::tool_runner::resolve_effective(
                &*dispatcher.resolver,
                &dispatcher.chain,
                &tool,
                &input,
            )
            .await,
            &dispatcher.mode,
        );
        crate::script::run_rhai(dispatcher, launch, self_perm, request_id, input, run_stop).await;
    }));
    if !background {
        cancels.register(
            &reg_session,
            TaskCanceller::script(handle.abort_handle(), stop),
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn permission(
    ctx: &LadderCtx,
    spawn_guard: &SpawnGuard,
    overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
    tool: String,
    // The call as the model emitted it when core unwrapped an `mcp`
    // envelope (ADR-0226) — for the duplicate-key re-scan.
    envelope: Option<ToolEnvelope>,
    session: SessionId,
    seq: u64,
    request_id: String,
    input: String,
) {
    let dispatcher = dispatcher(ctx, spawn_guard, overlays, &session);
    let call = Call {
        request_id,
        tool,
        input,
        envelope,
    };
    // Registered so a `Stop` aborts the task mid-execution: dropping the
    // future drops an exec tool's child and fires its process-group SIGKILL
    // guard (#167/#168). The ToolExec seq rides the task so a file change
    // names its exact log record (ADR-0212).
    let handle = tokio::spawn(crate::file_backup::with_exec_seq(
        seq,
        dispatcher.dispatch(call),
    ));
    ctx.cancels
        .register(&session, TaskCanceller::task(handle.abort_handle()));
}
