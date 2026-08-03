//! `propose_plan` — the plan agent's one tool: submit a plan (`content` XOR
//! `path`) for the user's approval (#141, ADR-0042; #513, ADR-0145, which
//! removes the separate `update_plan` snapshot tool and amends ADR-0138's
//! fire-and-forget framing to a review loop).
//!
//! A plan is a **file** under `.entanglement/plans/`, not an in-memory
//! snapshot: `content` materializes (or overwrites) one there and `path` binds
//! to an existing `.md` file the agent already wrote or the user seeded (parse/
//! resolve logic lives in the sibling `resolve` module, split out to stay
//! under the file-size cap). Either way the resolved content rides back as an
//! `OutEvent::Plan` snapshot for the plan session itself (the wire event
//! non-TUI heads render) and the subsequent `ToolRequest` — the tool is
//! intercepted on [`OutEvent::ToolExec`] — like `ask_user` (ADR-0027) — and
//! **force-parked on the `Ask` path unconditionally**. A permission profile
//! can never `Allow` it, because user approval *is* the tool's semantics. A
//! malformed call (both/neither of `content`/`path`, a missing/non-`.md`
//! `path`, or a stale `path` — see the staleness guard below) is refused
//! immediately instead, with no approval prompt: it is a self-correctable
//! model error, not a decision for the human.
//!
//! **Staleness guard** (`path` mode only): the session must be the last party
//! known to have touched the bound file — tracked by [`crate::plan_files`] off
//! this module's own reads/writes plus the executor's `FileChange` audit for a
//! matching in-session `edit`/`write`/`apply_patch`. A file the *user* edited
//! out of band since is refused with a re-read-required error.
//!
//! - **Approve** → [`run_propose_plan`] spawns a **sponsored** `build` child of
//!   the plan session (ADR-0138): a child with a parent-child link (so the
//!   result flows back and the plan agent can cycle) but whose permission
//!   resolution stops at the child — it runs with `build`'s own write-tool
//!   permissions, no ancestor clamp. The plan agent parks on `WaitingAgent`
//!   (ADR-0139) while the build runs; the build's **full final report** folds
//!   back as the `propose_plan` tool result, so the plan agent has the
//!   implementation outcome in context and is expected to review it against
//!   the plan, update the plan file (e.g. phase checkboxes) via `write`/`edit`,
//!   and either `propose_plan` the next phase or report done — a multi-phase
//!   plan → build → review loop. The build child sees the accepted plan in its
//!   outline via an `OutEvent::Plan` snapshot (B6).
//! - **Reject + reason** → the existing rejection fold-back (`tool
//!   \`propose_plan\` rejected: <reason>`); the model revises and re-proposes in
//!   the same turn, no new code.
//!
//! The build session is a **sponsored child** of plan, not a fresh root (the
//! pre-ADR-0138 shape): a parent link would historically clamp `build` to
//! `plan`'s read-only tool set (ADR-0024) and drain the plan root's spawn
//! budget (ADR-0023). Sponsorship exempts it from both — authorization is user
//! plan approval, not inheritance — while preserving the link the cycle needs.
//!
//! **Stop while parked on the blocking build wait**: the whole `run_propose_plan`
//! task — the Ask-wait *and* the post-approval build wait — is registered with
//! [`crate::cancel::CancelRegistry`] by its caller (`tool_runner`'s
//! `Intercept::ProposePlan` arm), so a `Stop` targeting the plan session alone
//! aborts the wait and detaches: the build child keeps running untouched (it
//! has its own independent turn). A head that wants to stop the child too
//! sends it a second, explicit `Stop` — no new protocol surface needed, this
//! module's only job is making "stop the plan session" not also imply "kill
//! the child".

use std::path::PathBuf;
use std::sync::Arc;

use entanglement_core::{AgentProfile, AgentState, Holly, InMsg, OutEvent, SessionId, ToolSpec};
use tokio::sync::broadcast::Receiver;

use crate::agent_poll::AgentRegistry;
use crate::pending::{self, PendingDecisions};
use crate::plan_files::PlanFileRegistry;
use crate::seam;
use crate::tool_names::PROPOSE_PLAN_TOOL;

mod resolve;
use resolve::{parse_plan_input, resolve_plan};

/// The profile a handoff mints its fresh session under: the plan is accepted into
/// a `build` session (ADR-0042).
pub const HANDOFF_PROFILE: &str = "build";

/// The `propose_plan` tool schema. Advertised only to a profile that explicitly
/// allowlists `propose_plan` via
/// [`EngineConfig::profile_tool_specs`][entanglement_core::EngineConfig] (#231,
/// ADR-0049) — the default-closed plan-authorship gate, so the tool never leaks
/// to an inherit-all profile.
pub fn propose_plan_spec() -> ToolSpec {
    ToolSpec::with_schema(
        PROPOSE_PLAN_TOOL,
        "Submit the plan for the user's acceptance. Provide exactly one of \
         `content` (plan markdown to materialize as a new plan file under \
         .entanglement/plans/) or `path` (an existing .md plan file to submit \
         as-is — it must be the file you most recently read, wrote, or edited; \
         a file changed by someone else since is refused, re-read it first). \
         The user approves or rejects: on approval the plan is handed off to a \
         `build` session and its full final report is returned as this call's \
         result — review it against the plan, update the plan file (e.g. phase \
         checkboxes), and call propose_plan again for the next phase, or stop \
         once the plan is fully implemented. On rejection you receive their \
         reason and should revise and call propose_plan again.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The plan document, in markdown. Materializes (or overwrites) the plan file."
                },
                "path": {
                    "type": "string",
                    "description": "Path to an existing .md plan file to submit as-is, instead of `content`."
                }
            }
        }),
    )
}

/// The per-profile `propose_plan` specs (#141, ADR-0042; #231, ADR-0049): the
/// tool advertised to a session running under `profile`, gated by explicit
/// allowlist membership so it never leaks to an inherit-all profile. Empty for
/// a profile that does not opt in. Appended to
/// [`EngineConfig::profile_tool_specs`][entanglement_core::EngineConfig]
/// alongside the spawn family; core's `run_turn` filters it through the #116 tool
/// mask, which the same allowlist entry satisfies.
pub fn specs_for(profile: &AgentProfile) -> Vec<ToolSpec> {
    if crate::plan_tasks::explicitly_allowlists(profile, PROPOSE_PLAN_TOOL) {
        vec![propose_plan_spec()]
    } else {
        Vec::new()
    }
}

/// Compose the first user message of the handoff `build` session from an accepted
/// plan. The plan markdown is embedded **verbatim** (the build agent implements
/// exactly what the user approved); only a short framing preamble is added.
pub fn wrap_plan(plan: &str) -> String {
    format!(
        "The following implementation plan has been reviewed and approved by the \
         user. Implement it.\n\n{plan}"
    )
}

/// Orchestrate one `propose_plan` call: resolve its `content`/`path` input to a
/// file, surface it as a standard approval prompt, and park for the head's
/// decision.
///
/// A resolution failure (bad input, missing file, staleness) replies
/// immediately with no `ToolRequest` ever emitted — never registers a pending
/// waiter for it, so nothing is left dangling. Otherwise registers the waiter
/// with the lag-proof [`PendingDecisions`] registry (#156) *before* emitting
/// the request, so a fast decision routes to this park rather than racing a
/// per-task broadcast subscription that could lag and drop it. A `Stop` while
/// parked unwinds silently: core's turn cancels on the same `Stop`, so no
/// `ToolResult` is owed.
///
/// On **Approve**, launches the pre-resolved sponsored `build` `child`
/// (ADR-0138): the tool executor already ran the SpawnGuard sponsor check and
/// recorded the parent link, so this function only sends the `InMsg::Spawn`,
/// parks the plan session on `WaitingAgent` (ADR-0139), and folds the build's
/// answer back as the `propose_plan` tool result. The child runs with
/// `build`'s own write-tool permissions (no ancestor clamp — authorization is
/// user plan approval).
#[allow(clippy::too_many_arguments)]
pub async fn run_propose_plan(
    holly: Holly,
    pending: PendingDecisions,
    registry: AgentRegistry,
    events_rx: Receiver<OutEvent>,
    plan_files: Arc<PlanFileRegistry>,
    root: PathBuf,
    session: SessionId,
    request_id: String,
    input: String,
    child: SessionId,
) {
    let plan_input = match parse_plan_input(&input) {
        Ok(p) => p,
        Err(msg) => {
            seam::reply(&holly, session, request_id, msg).await;
            return;
        }
    };
    let resolution = match resolve_plan(&root, &session, &plan_files, plan_input) {
        Ok(r) => r,
        Err(msg) => {
            seam::reply(&holly, session, request_id, msg).await;
            return;
        }
    };

    // Surface the resolved plan on the plan session itself (#513): non-TUI
    // heads render it immediately, and the TUI's status-line/editor
    // affordance binds to `path`. Mints a fresh per-session seq (#157).
    holly.emit_for_session(&session, |seq| OutEvent::Plan {
        session: session.clone(),
        seq,
        content: resolution.content.clone(),
        path: resolution.rel_path.clone(),
    });

    // Register before emitting so the inbound router can never resolve the
    // decision ahead of this waiter (#156).
    let rx = pending.register(&session, &request_id);

    // A standard `ToolRequest` — the head renders the usual approve/reject
    // prompt. `input` carries the *resolved* content (not the model's raw
    // `content`-XOR-`path` call) so a `path`-mode approval still shows the
    // full plan text, not just a filename — `tui::tool_render`'s
    // `propose_plan` arm reads this same JSON shape.
    holly.emit_for_session(&session, |seq| OutEvent::ToolRequest {
        session: session.clone(),
        seq,
        request_id: request_id.clone(),
        tool: PROPOSE_PLAN_TOOL.to_string(),
        input: serde_json::json!({
            "content": resolution.content,
            "path": resolution.rel_path,
        })
        .to_string(),
    });
    holly.emit_status(&session, AgentState::WaitingApproval);

    match pending::await_decision(rx).await {
        seam::Decision::Approve { .. } => {
            // Launch the sponsored build child (ADR-0138).
            launch_sponsored_build(
                holly,
                registry,
                events_rx,
                session,
                request_id,
                resolution.content,
                resolution.rel_path,
                child,
            )
            .await;
        }
        seam::Decision::Reject { reason } => {
            set_thinking(&holly, &session);
            let output = format!(
                "tool `{PROPOSE_PLAN_TOOL}` rejected (plan file: {}): {}",
                resolution.rel_path,
                reason.as_deref().unwrap_or("user")
            );
            seam::reply(&holly, session, request_id, output).await;
        }
        // `Stop` (and a closed inbox) unwind silently; `Answer`/`Retract`/
        // `Replace` never target a `propose_plan` request id (they are
        // `ask_user`-only, #515).
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => {}
    }
}

/// Launch the sponsored `build` `child` of the plan `session`, park until it
/// finishes, and fold its answer back as the `propose_plan` tool result
/// (ADR-0138). The `child` id and its sponsored parent link were already
/// resolved by the tool executor's single-threaded loop (so the SpawnGuard
/// mutation stays race-free); this function owns the async half — sending the
/// `InMsg::Spawn`, parking the plan session on `WaitingAgent` (ADR-0139), and
/// folding the answer back. Emits an `OutEvent::Plan` snapshot for the build
/// child so it sees the accepted plan in its outline (B6).
#[allow(clippy::too_many_arguments)]
async fn launch_sponsored_build(
    holly: Holly,
    registry: AgentRegistry,
    mut events_rx: Receiver<OutEvent>,
    session: SessionId,
    request_id: String,
    plan: String,
    plan_path: String,
    child: SessionId,
) {
    let prompt = wrap_plan(&plan);
    // Register the child in the agent-poll registry *before* sending Spawn so a
    // poll can never precede the handle (mirrors `launch` in subagent.rs).
    let (status_tx, started) = registry.register(child.clone(), session.clone());

    if holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(session.clone()),
            predecessor: None,
            agent: HANDOFF_PROFILE.to_string(),
            prompt: prompt.clone(),
            user: None,
        })
        .await
        .is_err()
    {
        registry.forget(&child);
        set_thinking(&holly, &session);
        seam::reply(
            &holly,
            session,
            request_id,
            "sponsored build spawn failed: engine inbox closed".to_string(),
        )
        .await;
        return;
    }

    // B6: surface the accepted plan to the build child as an `OutEvent::Plan`
    // snapshot, so its outline renders the plan it's implementing.
    holly.emit_for_session(&child, |seq| OutEvent::Plan {
        session: child.clone(),
        seq,
        content: plan.clone(),
        path: plan_path.clone(),
    });

    // The plan session parks on the child's result — surface that as a distinct
    // state (ADR-0139).
    holly.emit_status(&session, AgentState::WaitingAgent);

    // Watch the child's event stream and accumulate its answer — parked past an
    // errored build turn with no usable answer instead of concluding on top of a
    // failed build (#562, see `collect_child_answer`'s doc). A `Stop` on
    // `session` aborts this whole task (the caller registers it with
    // `CancelRegistry`) — the child keeps running untouched, i.e. "detach" is
    // this function simply never resuming.
    let answer =
        crate::subagent::collect_child_answer(&holly, &session, &mut events_rx, &child).await;
    let elapsed = started.elapsed();
    let _ = status_tx.send(crate::agent_poll::AgentStatus::Complete {
        answer: answer.clone(),
        elapsed,
    });

    // Fold the build's answer back as the propose_plan tool result, so the plan
    // agent has the implementation outcome in context and can revise +
    // re-propose (cycle, ADR-0138). Names the plan file (#513) so the agent
    // knows where to apply its review-loop edits without having to recall it.
    set_thinking(&holly, &session);
    let output = format!(
        "plan file: {plan_path}\n\nbuild completed in {:.1}s:\n\n{answer}",
        elapsed.as_secs_f64()
    );
    seam::reply(&holly, session, request_id, output).await;
}

fn set_thinking(holly: &Holly, session: &SessionId) {
    holly.emit_status(session, AgentState::Thinking);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_plan_embeds_the_plan_verbatim() {
        let plan = "# Plan\n1. Add the tool\n2. Wire the handoff";
        let msg = wrap_plan(plan);
        assert!(
            msg.contains(plan),
            "the accepted plan must reach the build session verbatim: {msg}"
        );
        assert!(msg.starts_with("The following implementation plan"));
    }

    #[test]
    fn specs_advertised_only_to_explicit_allowlisters() {
        // Plan authorship is default-closed (#231, ADR-0049): only a profile that
        // explicitly allowlists `propose_plan` gets the spec. The built-in `plan`
        // profile does (its allowlist lists it); `build` (inherit-all) and
        // `explore` (read trio) do not.
        let reg = crate::agents::built_in_registry().expect("built-in agents must parse");
        assert!(
            specs_for(reg.get("build").unwrap()).is_empty(),
            "an inherit-all profile gets no propose_plan spec"
        );
        assert!(specs_for(reg.get("explore").unwrap()).is_empty());
        let plan_specs = specs_for(reg.get("plan").unwrap());
        assert_eq!(plan_specs.len(), 1);
        assert_eq!(plan_specs[0].name, PROPOSE_PLAN_TOOL);
    }

    #[test]
    fn spec_has_no_required_array_since_content_and_path_are_mutually_exclusive() {
        let spec = propose_plan_spec();
        assert_eq!(spec.name, PROPOSE_PLAN_TOOL);
        assert!(spec.schema.get("required").is_none());
        assert!(spec.schema["properties"].get("content").is_some());
        assert!(spec.schema["properties"].get("path").is_some());
    }
}
