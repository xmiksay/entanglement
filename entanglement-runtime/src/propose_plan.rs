//! `propose_plan` — the plan agent's *finalize* step (#141, ADR-0042, amended
//! by ADR-0138).
//!
//! `update_plan` (#140) records working snapshots; `propose_plan` asks the user
//! to **accept** a finished plan. Acceptance rides the existing tool-approval
//! round-trip (#59) instead of a new protocol message: the tool is intercepted
//! on [`OutEvent::ToolExec`] — like `ask_user` (ADR-0027) — and **force-parked on
//! the `Ask` path unconditionally**. A permission profile can never `Allow` it,
//! because user approval *is* the tool's semantics.
//!
//! - **Approve** → [`run_propose_plan`] spawns a **sponsored** `build` child of
//!   the plan session (ADR-0138): a child with a parent-child link (so the
//!   result flows back and the plan agent can cycle) but whose permission
//!   resolution stops at the child — it runs with `build`'s own write-tool
//!   permissions, no ancestor clamp. The plan agent parks on `WaitingAgent`
//!   (ADR-0139) while the build runs; the build's answer folds back as the
//!   `propose_plan` tool result, so the plan agent has the implementation
//!   outcome in context and can revise + re-propose (cycle). The build child
//!   sees the accepted plan in its outline via an `OutEvent::Plan` snapshot
//!   (B6).
//! - **Reject + reason** → the existing rejection fold-back (`tool
//!   \`propose_plan\` rejected: <reason>`); the model revises and re-proposes in
//!   the same turn, no new code.
//!
//! The build session is a **sponsored child** of plan, not a fresh root (the
//! pre-ADR-0138 shape): a parent link would historically clamp `build` to
//! `plan`'s read-only tool set (ADR-0024) and drain the plan root's spawn
//! budget (ADR-0023). Sponsorship exempts it from both — authorization is user
//! plan approval, not inheritance — while preserving the link the cycle needs.

use entanglement_core::{AgentProfile, AgentState, Holly, InMsg, OutEvent, SessionId, ToolSpec};
use tokio::sync::broadcast::Receiver;

use crate::agent_poll::AgentRegistry;
use crate::pending::{self, PendingDecisions};
use crate::seam;
use crate::tool_names::PROPOSE_PLAN_TOOL;

/// The profile a handoff mints its fresh session under: the plan is accepted into
/// a `build` session (ADR-0042).
pub const HANDOFF_PROFILE: &str = "build";

/// The `propose_plan` tool schema. Advertised only to a profile that explicitly
/// allowlists `propose_plan` via
/// [`EngineConfig::profile_tool_specs`][entanglement_core::EngineConfig] (#231,
/// ADR-0049) — the same default-closed plan-authorship gate as `update_plan`, so
/// the tool never leaks to an inherit-all profile.
pub fn propose_plan_spec() -> ToolSpec {
    ToolSpec::with_schema(
        PROPOSE_PLAN_TOOL,
        "Submit the finished plan for the user's acceptance. Use this once the \
         plan is complete (keep using update_plan for working snapshots). The \
         user approves or rejects: on approval the plan is handed off to a fresh \
         build session to be implemented; on rejection you receive their reason \
         and should revise and call propose_plan again.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The final plan document, in markdown."
                }
            },
            "required": ["plan"]
        }),
    )
}

/// The per-profile `propose_plan` specs (#141, ADR-0042; #231, ADR-0049): the
/// tool advertised to a session running under `profile`, gated by explicit
/// allowlist membership — the same default-closed plan-authorship gate as
/// `update_plan`, so it never leaks to an inherit-all profile. Empty for a
/// profile that does not opt in. Appended to
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

/// Extract the `plan` markdown from a `propose_plan` tool input. A malformed or
/// bare-string input degrades to the raw text, so a scripted backend still yields
/// a plan instead of a schema error (mirrors `ask_user`'s tolerance).
pub fn parse_plan(input: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => v
            .get("plan")
            .and_then(|p| p.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string()),
        Err(_) => input.to_string(),
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

/// Orchestrate one `propose_plan` call: surface the plan as a standard approval
/// prompt and park for the head's decision.
///
/// Registers the waiter with the lag-proof [`PendingDecisions`] registry (#156)
/// *before* emitting the request, so a fast decision routes to this park rather
/// than racing a per-task broadcast subscription that could lag and drop it. A
/// `Stop` while parked unwinds silently: core's turn cancels on the same `Stop`,
/// so no `ToolResult` is owed.
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
    session: SessionId,
    request_id: String,
    input: String,
    child: SessionId,
) {
    // Register before emitting so the inbound router can never resolve the
    // decision ahead of this waiter (#156).
    let rx = pending.register(&session, &request_id);

    // Parse the plan up front: the tool input is moved into the ToolRequest
    // emit below, but the plan is needed only on the Approve branch.
    let plan = parse_plan(&input);

    // A standard `ToolRequest` — the head renders the usual approve/reject prompt.
    // Mints a fresh per-session seq (#157) rather than reusing the `ToolExec` seq.
    holly.emit_for_session(&session, |seq| OutEvent::ToolRequest {
        session: session.clone(),
        seq,
        request_id: request_id.clone(),
        tool: PROPOSE_PLAN_TOOL.to_string(),
        input,
    });
    holly.emit_status(&session, AgentState::WaitingApproval);

    match pending::await_decision(rx).await {
        seam::Decision::Approve { .. } => {
            // Launch the sponsored build child (ADR-0138).
            launch_sponsored_build(holly, registry, events_rx, session, request_id, plan, child)
                .await;
        }
        seam::Decision::Reject { reason } => {
            set_thinking(&holly, &session);
            let output = format!(
                "tool `{PROPOSE_PLAN_TOOL}` rejected: {}",
                reason.as_deref().unwrap_or("user")
            );
            seam::reply(&holly, session, request_id, output).await;
        }
        // `Stop` (and a closed inbox) unwind silently; `Answer` never targets a
        // `propose_plan` request id.
        seam::Decision::Stop | seam::Decision::Answer { .. } => {}
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
async fn launch_sponsored_build(
    holly: Holly,
    registry: AgentRegistry,
    mut events_rx: Receiver<OutEvent>,
    session: SessionId,
    request_id: String,
    plan: String,
    child: SessionId,
) {
    let prompt = wrap_plan(&plan);
    // Register the child in the agent-poll registry *before* sending Spawn so a
    // poll can never precede the handle (mirrors `launch` in subagent.rs).
    let (status_tx, started) = registry.register(child.clone());

    if holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(session.clone()),
            predecessor: None,
            agent: HANDOFF_PROFILE.to_string(),
            prompt: prompt.clone(),
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
    });

    // The plan session parks on the child's result — surface that as a distinct
    // state (ADR-0139).
    holly.emit_status(&session, AgentState::WaitingAgent);

    // Watch the child's event stream and accumulate its answer.
    let answer = crate::subagent::collect_child_answer(&mut events_rx, &child).await;
    let elapsed = started.elapsed();
    let _ = status_tx.send(crate::agent_poll::AgentStatus::Complete {
        answer: answer.clone(),
        elapsed,
    });

    // Fold the build's answer back as the propose_plan tool result, so the plan
    // agent has the implementation outcome in context and can revise +
    // re-propose (cycle, ADR-0138).
    set_thinking(&holly, &session);
    let output = format!(
        "build completed in {:.1}s:\n\n{answer}",
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
    fn parse_plan_reads_json_field() {
        assert_eq!(
            parse_plan(r##"{"plan":"# Do X\n1. step"}"##),
            "# Do X\n1. step"
        );
    }

    #[test]
    fn parse_plan_degrades_to_raw_string() {
        assert_eq!(parse_plan("just a plan"), "just a plan");
    }

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
        let reg = crate::agents::built_in_registry();
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
    fn spec_requires_the_plan_field() {
        let spec = propose_plan_spec();
        assert_eq!(spec.name, PROPOSE_PLAN_TOOL);
        let required = spec.schema["required"].as_array().unwrap();
        assert!(required.iter().any(|r| r == "plan"));
    }
}
