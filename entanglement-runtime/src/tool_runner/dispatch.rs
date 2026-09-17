//! The executor's generic tool-call primitives (issue #451, split out of
//! `tool_runner.rs`): [`dispatch`] resolves one `ToolExec`'s permission — via
//! [`grade::apply_grant`]/[`grade::resolve_effective`] — and runs, denies, or
//! parks it; [`decision::await_decision`] resolves a parked approval;
//! [`execute::run_and_reply`] is the run-against-the-registry step both
//! reach. These grew every time a later stage added a gate (grants,
//! escape-root, mode limits, argument validation); the executor loop above
//! (event folding, registry mirrors, `LadderCtx`) never touches them
//! directly — only `ladder::graded` calls into [`dispatch`].

mod decision;
mod execute;
pub(crate) mod grade;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{AgentState, Holly, OutEvent, Permission, PermissionProfile, SessionId};

use crate::capability::{self, Capability};
use crate::hooks::Hooks;
use crate::mcp::{ActiveServers, AvailableMcp};
use crate::permission::clamp_to_base;
use crate::permission_path::grading_arg;
use crate::policy::{GrantStore, PermissionResolver};
use crate::skills::SkillRegistry;
use crate::tool_names::REQUEST_MODE_TOOL;
use crate::tools::{SharedRegistry, ToolRegistry};
use crate::{arg_validate, run_limits, seam, tool_advertising};

use super::EscapeRoot;

/// Resolve one `ToolExec` per its permission and reply with a `ToolResult`.
///
/// The grade comes from the pluggable [`PermissionResolver`] (#311) — the
/// default [`ModeResolver`][crate::policy::ModeResolver] grades from
/// the session's permission mode (ADR-0207 stage 4) — clamped least-privilege
/// across the call's ancestor `chain` (the sub-agent ceiling, ADR-0024) and
/// upgraded from `Ask` to `Allow` by an existing [`GrantStore`] grant scoped
/// to `mode` (ADR-0207 §8). The DB-backed resolve runs here in the detached
/// task, not the loop.
///
/// A `pre_tool_use` hook (#199) can **veto** the call: a non-zero-exit hook
/// short-circuits with a denial `ToolResult`, so the tool neither prompts nor
/// runs. Cleared hooks fall through to the normal `Allow | Ask | Deny` dispatch.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch(
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
    // Whether the nearest ancestor-chain link with an overlay *opinion* about
    // this tool is a **deny** (#634, ADR-0207 §8 restore — gap 2 of stage
    // 4b): resolved by the caller alongside `overlay_entry` above, from the
    // same `overlays`/`chain` the loop already holds. Checked before the
    // mode grade even runs, and before `overlay_entry`'s enable materializes
    // a grade — a deny withdraws the tool from the session outright, the way
    // the retired `tool_mask_source` used to.
    overlay_denied: bool,
    ceiling: &PermissionProfile,
    // Pre-dispatch argument-validation state (#560, ADR-0196 §6): the
    // delivered-schema dedup (shares `advertising.discovered` with `describe`,
    // ADR-0196 §4) and the loop-breaker's per-session last-call tracker.
    advertising: &tool_advertising::AdvertisingState,
    validation: &arg_validate::LoopBreaker,
    // ADR-0201's dispatch-time lazy MCP re-enable: the live registry (to
    // register into, and to re-snapshot from on success — `tools` above is
    // an already-cloned snapshot that a fresh registration is invisible to),
    // the availability roster + connected-server map `enable_for_session`
    // needs, and the endpoint-pool client its connect rides.
    registry: &SharedRegistry,
    mcp_avail: &AvailableMcp,
    mcp_active: &ActiveServers,
    http: Option<&entanglement_core::HttpClient>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
    // The session's current permission mode (ADR-0207 stage 4), resolved by
    // the caller before spawning (mirrors `overlay_entry`/`chain` above) —
    // threaded through to the grant lookup/record so a grant is matched
    // and recorded against the mode it was actually earned under (§8).
    mode: String,
    // `mode`'s resolved `Limits` (ADR-0207 §11, stage 5c), resolved by the
    // caller alongside `mode` itself — governs whether a bare `Ask` collapses
    // to a denial (`run_limits::collapses_ask`) and how long a parked
    // approval waits (`run_limits::timeout`).
    limits: crate::mode::Limits,
    // Per-session, per-turn repeat-denial tracker (ADR-0207 §11): a second
    // identical collapsed-`Ask` denial parks an approval instead of refusing
    // silently again.
    denials: &crate::run_limits::DenialTracker,
) {
    // A hallucinated tool name can never execute, so reject it *before* the
    // ladder runs (#437): otherwise an `Ask` grade prompts the user to approve
    // a call that can only fail, `pre_tool_use` vetoes a call that was never
    // executable, and an `Always`-scoped approval could record a grant for a
    // tool that doesn't exist. Uses the same freshly-snapshotted `tools`
    // `dispatch` already received, so a live `McpAdd`/`McpRemove` (#372) is
    // honored exactly as execution itself would see it. `update_tasks` is a
    // runtime state tool with no registry entry (#231, ADR-0049) — `run_and_reply`
    // handles it separately — and `request_mode` (ADR-0207 §10) is likewise a
    // pure orchestration pseudo-tool with no registry entry, handled specially
    // just below in the `Control` bypass — so both are exempt from this
    // registry check.
    //
    // ADR-0201: an unregistered `mcp__<server>__*` name is not necessarily a
    // hallucination — a resumed session's replay restores its tool-overlay/
    // permission state (so the call reaches here, never masked) but MCP
    // registration is process-lifetime, never persisted, so nothing
    // re-registers on resume. Self-heal by consulting the same three-state
    // tier `mcp_enable`/`/enable mcp` do before ever reporting "unknown":
    // reserve that message strictly for a name matching no registered tool
    // AND no configured/bundled server in any tier.
    let mut refreshed_tools: Option<ToolRegistry> = None;
    if !tools.contains(&tool)
        && !crate::plan_tasks::is_state_tool(&tool)
        && tool != REQUEST_MODE_TOOL
    {
        match crate::mcp::available::server_name_of(&tool) {
            Some(server) => {
                match crate::mcp::available::try_lazy_reenable(
                    mcp_avail, server, &session, registry, mcp_active, http,
                )
                .await
                {
                    crate::mcp::available::LazyReenableOutcome::Enabled => {
                        // The enable registered into the *live* `registry`,
                        // invisible to the already-cloned `tools` snapshot —
                        // re-fetch it and let the rest of this function run
                        // against the fresh view (alias rewrite, grading,
                        // escape-root, hooks, the approval round-trip all
                        // still apply below, exactly as if the tool had
                        // been registered all along).
                        let snap = registry
                            .read()
                            .expect("tool registry lock poisoned")
                            .clone();
                        if !snap.contains(&tool) {
                            // Shouldn't happen (enable_for_session just
                            // registered it) — fail safe, not panic.
                            let output = snap.unknown_tool_message(&tool);
                            seam::reply(holly, session, request_id, output, true).await;
                            return;
                        }
                        refreshed_tools = Some(snap);
                    }
                    crate::mcp::available::LazyReenableOutcome::Disabled => {
                        let output = crate::mcp::available::disabled_decline(server);
                        seam::reply(holly, session, request_id, output, true).await;
                        return;
                    }
                    crate::mcp::available::LazyReenableOutcome::Failed(msg) => {
                        seam::reply(holly, session, request_id, msg, true).await;
                        return;
                    }
                    crate::mcp::available::LazyReenableOutcome::Unknown => {
                        let output = tools.unknown_tool_message(&tool);
                        seam::reply(holly, session, request_id, output, true).await;
                        return;
                    }
                }
            }
            None => {
                let output =
                    tool_advertising::unknown_tool_reply(advertising, &session, tools, &tool);
                seam::reply(holly, session, request_id, output, true).await;
                return;
            }
        }
    }
    let tools = refreshed_tools.as_ref().unwrap_or(tools);
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
    // ADR-0207 §3: a `Capability::Control`-only tool reads/orchestrates
    // session state and cannot itself read, write or execute anything on the
    // host, so it is never graded — checked by *capability*, not by adding
    // another `Intercept` route to remember, so it automatically covers
    // `update_tasks`/`load_skill`/`mcp_enable`/`request_mode` with nothing to
    // update here for *grading*. `mcp_enable` is the one that looks like it
    // should escalate: enabling a server only makes its tools *dispatchable*,
    // and every one of those is still graded on its own merits at its own
    // call, so this can't widen anything. Bypasses the overlay too
    // (`overlay_denied` below never reached) — Control is "never graded"
    // outright, the same posture `Intercept::Discover`'s sibling routes
    // already take.
    if capability::capability_of(&tool, tools) == Some([Capability::Control].as_slice()) {
        // `request_mode` (ADR-0207 §10) is the one Control tool whose
        // semantics — like `propose_plan`'s — *require* an unconditional
        // approval park: widening a session's own authority is a decision
        // only the user makes, never one `run_and_reply`'s straight-through
        // execution can grant. A name check here, not a second `Intercept`
        // route, is what keeps this a capability-driven bypass rather than
        // another routing table entry to remember.
        if tool == REQUEST_MODE_TOOL {
            crate::request_mode::run_request_mode(holly, pending, session, request_id, input, mode)
                .await;
            return;
        }
        execute::run_and_reply(
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
        return;
    }
    // A matching overlay **deny** entry withdraws the tool from the session
    // outright (#634, ADR-0207 §8's restore of the retired `tool_mask_source`
    // behavior) — checked before the mode grade runs, and before hooks, since
    // the tool doesn't "exist" for this session at all right now. An overlay
    // **enable** still overrides even a mode `deny` (`overlay_entry` below,
    // materialized further down) — that asymmetry is deliberate: the user's
    // own `/enable`/`/disable` is trusted-frame-only (ADR-0177), so the model
    // can reach neither directly.
    if overlay_denied {
        let output =
            format!("tool `{tool}` withdrawn for this session by /disable — /enable to restore");
        seam::reply(holly, session, request_id, output, true).await;
        return;
    }
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
            let overlay_profile = crate::permission::overlay_entry_grade(&tool, &entry);
            let grade = crate::permission_bash::resolve_scoped_bash_aware(
                &overlay_profile,
                &tool,
                arg.as_deref(),
                workdir.as_deref(),
            );
            let capabilities = crate::capability::capability_of(&tool, tools).unwrap_or(&[]);
            clamp_to_base(
                grade,
                ceiling,
                &tool,
                capabilities,
                arg.as_deref(),
                workdir.as_deref(),
            )
        }
        None => grade::resolve_effective(resolver, chain, &tool, &input).await,
    };
    let perm = grade::apply_grant(grants, &session, &tool, arg.as_deref(), base_perm, &mode);
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
            execute::run_and_reply(
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
            // ADR-0207 §4: a mode `deny` is absolute — no prompt — and the
            // message names the mode and the way out, so the model (or the
            // user reading the transcript) knows this isn't a bug to retry
            // but a posture to switch out of.
            let output = format!("tool `{tool}` denied by mode `{mode}` — use /mode to switch");
            seam::reply(holly, session, request_id, output, true).await;
        }
        // ADR-0207 §11: an unattended mode (a finite `question_timeout`) has
        // no one to prompt, so a bare `Ask` grade collapses to an immediate
        // denial rather than parking — *unless* this exact `(tool, arg)` was
        // already denied once this turn, in which case a model insisting
        // twice may genuinely need it, so the repeat falls through to the
        // ordinary park below (still bounded by the same timeout). The guard
        // records the denial as a side effect, so it never double-charges: a
        // first call denies-and-records, a second sees the record and falls
        // to `_`.
        Permission::Ask
            if run_limits::collapses_ask(&limits)
                && !denials.record_repeat(&session, &tool, arg.as_deref()) =>
        {
            let output = format!(
                "tool `{tool}` denied — mode `{mode}` is unattended (question_timeout \
                 {}s, no one to prompt); call it again to request a one-time approval",
                limits.question_timeout
            );
            seam::reply(holly, session, request_id, output, true).await;
        }
        // Either the mode said `Ask` (an attended mode, or a collapsed
        // mode's second identical call), or an out-of-root access forced one.
        _ => {
            // Register the waiter *before* prompting (#156) so the inbound router
            // can never process the approval before this park exists — the
            // lag-proof successor to the old "subscribe before prompting"
            // discipline. The prompt mints a **fresh** per-session seq (#157) from
            // the parked session's shared counter, so `(session, seq)` stays unique
            // instead of reusing the `ToolExec` seq.
            let rx = pending.register(&session, &request_id, "tool", tool.clone());
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
            decision::await_decision(
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
                mode,
                run_limits::timeout(&limits),
            )
            .await;
        }
    }
}
