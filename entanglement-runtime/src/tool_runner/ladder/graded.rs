//! The permission-graded handlers of the interception ladder (issue #451,
//! split out of `ladder.rs`): `rhai` (behind the `rhai` feature) and the
//! generic `Permission` route are the two handlers
//! [`Intercept::bypasses_permission`][super::Intercept::bypasses_permission]
//! is `false` for — each resolves the call's grade through the pluggable
//! [`crate::policy::PermissionResolver`] before running anything.

use std::collections::HashMap;
#[cfg(feature = "rhai")]
use std::sync::atomic::AtomicBool;
#[cfg(feature = "rhai")]
use std::sync::Arc;

use entanglement_core::SessionId;

use crate::cancel::TaskCanceller;
use crate::permission::{ancestor_chain, overlay_denies, overlay_grade_entry};
#[cfg(feature = "rhai")]
use crate::permission_path::grading_arg;
use crate::seam;
use crate::subagent::SpawnGuard;

use super::LadderCtx;

#[cfg(feature = "rhai")]
pub(super) async fn rhai(
    ctx: &LadderCtx,
    spawn_guard: &SpawnGuard,
    overlays: &HashMap<SessionId, Vec<entanglement_core::ToolOverlayEntry>>,
    tool: String,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // `rhai`'s own grade (whether the model may invoke
    // it at all this call) and every binding call
    // inside the script (`BindingPolicy::decide`, in
    // `crate::script`) now resolve through the *same*
    // pluggable resolver + ancestor-chain clamp a
    // direct tool call uses (ADR-0207 stage 4b) —
    // `rhai` declares `Capability::Read|Write|Exec`
    // (`capability.rs`), so a mode denying any of
    // those denies `rhai` outright with no bespoke
    // rule, and there is no second `AgentProfile`-
    // chain grading path left to drift from it. The
    // resolve can hit a DB for a pluggable multi-
    // tenant resolver, so — mirroring the `Permission`
    // route above — it runs inside the detached task,
    // never this loop; only the cheap, synchronous
    // chain/overlay snapshots are taken here.
    // Root-relative arg normalization (#485, ADR-0125):
    // computed from the escape-root policy before it's
    // cloned/shadowed below, so an in-root absolute path
    // grades identically to its relative spelling here too.
    let arg = grading_arg(
        &tool,
        &input,
        ctx.escape_root.as_ref().map(|er| er.root.as_path()),
    );
    let escape_root = ctx.escape_root.clone();
    let chain = ancestor_chain(spawn_guard, &session);
    // The grant lookup/record is mode-scoped (ADR-0207
    // §8): an approval earned in one mode must not
    // silently apply in another.
    let call_mode = ctx
        .perm_modes
        .lock()
        .expect("permission-mode mutex poisoned")
        .get(&session)
        .cloned()
        .unwrap_or_default();
    let policy = crate::script::BindingPolicy::capture(
        spawn_guard,
        overlays,
        &session,
        &ctx.base,
        ctx.resolver.clone(),
        call_mode.clone(),
        escape_root.as_ref().map(|er| er.root.as_path()),
    );
    let resolver = ctx.resolver.clone();
    let grants = ctx.grants.clone();
    let pending = ctx.pending.clone();
    // Snapshot the registry *before* spawning (#372): a brief
    // read lock, never held across the script's `.await`, so a
    // concurrent tool registration/removal is invisible to a
    // script already in flight but picked up by the next one.
    let tools = ctx
        .tools
        .read()
        .expect("tool registry lock poisoned")
        .clone();
    // A scoped session's script sees its scope's cached MCP
    // tools, never the global set (#684) — cached-only: a
    // script call must not block this loop on a lazy connect.
    let tools = match &ctx.mcp_scopes {
        Some(scopes) => scopes.overlay_registry_cached(&session, tools),
        None => tools,
    };
    let holly = ctx.holly.clone();
    // The blocking engine can't be aborted, so pair the
    // task abort with a cooperative stop flag its progress
    // callback polls (#167).
    let stop = Arc::new(AtomicBool::new(false));
    let reg_session = session.clone();
    let run_stop = stop.clone();
    let scripts = ctx.scripts.clone();
    // A `background: true` script (#637, ADR-0185)
    // deliberately survives a session `Stop`, exactly
    // as a background `bash`/`call` job does — so it is
    // never registered with the canceller. Its only
    // kill is `poll`'s `kill: true`, which trips the
    // same `stop` flag via the script registry.
    let background = crate::script::is_background(&input);
    let cancels = ctx.cancels.clone();
    let handle = tokio::spawn(async move {
        let self_perm = crate::tool_runner::apply_grant(
            &*grants,
            &session,
            &tool,
            arg.as_deref(),
            crate::tool_runner::resolve_effective(&*resolver, &chain, &tool, &input).await,
            &call_mode,
        );
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

pub(super) async fn permission(
    ctx: &LadderCtx,
    spawn_guard: &SpawnGuard,
    overlays: &HashMap<SessionId, Vec<entanglement_core::ToolOverlayEntry>>,
    tool: String,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // Snapshot the ancestor chain *before* spawning so it
    // stays ordered with the lifecycle events above (and the
    // `ToolExec.agent` self-heal); the detached task resolves
    // each session's grade through the pluggable resolver
    // (#311) and clamps least-privilege across the chain, so
    // a child sub-agent can never exceed any ancestor (#77).
    // A root (no ancestors) resolves to its own grade; an
    // unseen session defaults to `Deny` (fail-closed, #156).
    // The DB-backed resolver runs in the task, never the loop.
    let chain = ancestor_chain(spawn_guard, &session);
    let resolver = ctx.resolver.clone();
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
    // its own.
    let overlay_entry = overlay_grade_entry(overlays, &chain, &tool);
    // The same chain walk, but for a **deny** opinion
    // (#634, gap 2 of ADR-0207 stage 4b): the nearest
    // link with *any* overlay opinion about this tool
    // wins, and if that opinion is deny the call is
    // declined flat in `dispatch`, ahead of even
    // `overlay_entry`'s enable materialization.
    let overlay_denied = overlay_denies(overlays, &chain, &tool);
    let ceiling = ctx.base.clone();
    // The session's current permission mode (ADR-0207
    // stage 4), threaded into `dispatch` for the
    // mode-scoped grant lookup/record (ADR-0207 §8).
    // An unseen session's empty string never matches
    // a real mode name, so it never wrongly upgrades
    // — the resolver's own fail-closed `Deny` for an
    // unseen session already refuses the call before
    // a grant could apply.
    let call_mode = ctx
        .perm_modes
        .lock()
        .expect("permission-mode mutex poisoned")
        .get(&session)
        .cloned()
        .unwrap_or_default();
    // The live registry, cloned (cheap `Arc`) *before*
    // the snapshot shadow below — ADR-0201's dispatch-
    // time lazy MCP re-enable needs the live handle to
    // register into and re-snapshot from, not the
    // pre-spawn owned clone `tools` becomes next.
    let registry = ctx.tools.clone();
    let mcp_avail = ctx.mcp_avail.clone();
    let mcp_active = ctx.mcp_active.clone();
    let mcp_http = ctx.mcp_http.clone();
    // Snapshot before spawning (#372) — see the Rhai arm above.
    let tools = ctx
        .tools
        .read()
        .expect("tool registry lock poisoned")
        .clone();
    let holly = ctx.holly.clone();
    let skills = ctx.skills.clone();
    let active_skill = ctx.active_skill.clone();
    let grants = ctx.grants.clone();
    let hooks = ctx.hooks.clone();
    let pending = ctx.pending.clone();
    let escape_root = ctx.escape_root.clone();
    let mcp_scopes = ctx.mcp_scopes.clone();
    let advertising = ctx.advertising.clone();
    let validation = ctx.validation.clone();
    // Register so a `Stop` aborts this task mid-execution:
    // aborting the future drops the exec tool's child,
    // firing its process-group SIGKILL guard (#167/#168).
    let reg_session = session.clone();
    let cancels = ctx.cancels.clone();
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
                        seam::reply(&holly, session, request_id, msg, true).await;
                        return;
                    }
                }
            }
            None => tools,
        };
        crate::tool_runner::dispatch(
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
            overlay_denied,
            &ceiling,
            &advertising,
            &validation,
            &registry,
            &mcp_avail,
            &mcp_active,
            mcp_http.as_ref(),
            session,
            request_id,
            tool,
            input,
            call_mode,
        )
        .await;
    });
    cancels.register(&reg_session, TaskCanceller::task(handle.abort_handle()));
}
