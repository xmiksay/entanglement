//! `propose_plan` — the plan agent's *finalize* step (#141, ADR-0042).
//!
//! `update_plan` (#140) records working snapshots; `propose_plan` asks the user
//! to **accept** a finished plan. Acceptance rides the existing tool-approval
//! round-trip (#59) instead of a new protocol message: the tool is intercepted
//! on [`OutEvent::ToolExec`] — like `ask_user` (ADR-0027) — and **force-parked on
//! the `Ask` path unconditionally**. A permission profile can never `Allow` it,
//! because user approval *is* the tool's semantics.
//!
//! - **Approve** → [`run_propose_plan`] records the plan with [`InMsg::SetPlan`]
//!   (engine state stays consistent for every head) and replies
//!   `ToolOutput("plan accepted by the user")`, so the plan agent learns the
//!   outcome and can end its turn. The head then performs the *handoff*: mint a
//!   fresh root `build` session whose first user message is the plan (see
//!   [`wrap_plan`]). The handoff is **head policy** — zero new protocol surface —
//!   so pipe/WS heads implement the same recipe (ADR-0042).
//! - **Reject + reason** → the existing rejection fold-back (`tool
//!   \`propose_plan\` rejected: <reason>`); the model revises and re-proposes in
//!   the same turn, no new code.
//!
//! The build session is a fresh **root**, not a child of the plan session: a
//! parent link would clamp build to plan's read-only tool set (#116) and drain
//! the plan root's spawn budget (ADR-0023), and accept is a transfer of authority
//! *from the user* — correctly modeled as a root (ADR-0042).

use entanglement_core::{AgentProfile, AgentState, Holly, InMsg, OutEvent, SessionId, ToolSpec};
use tokio::sync::broadcast::{error::RecvError, Receiver};

/// Tool name the plan agent calls to finalize and submit its plan for approval.
pub const PROPOSE_PLAN_TOOL: &str = "propose_plan";

/// The profile a handoff mints its fresh session under: the plan is accepted into
/// a `build` session (ADR-0042).
pub const HANDOFF_PROFILE: &str = "build";

/// The `propose_plan` tool schema. Advertised only to `owns_plan` profiles via
/// [`EngineConfig::profile_tool_specs`][entanglement_core::EngineConfig] (#140,
/// ADR-0041) — the same default-closed-authority gate as `update_plan`, so the
/// tool never leaks to an unmasked user profile.
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

/// The per-profile `propose_plan` specs (#141, ADR-0042): the tool advertised to
/// a session running under `profile`, gated by [`owns_plan`][AgentProfile] — the
/// same default-closed authority as `update_plan` (#140), so it never leaks to an
/// unmasked user profile. Empty for a profile that does not own the plan. Appended
/// to [`EngineConfig::profile_tool_specs`][entanglement_core::EngineConfig]
/// alongside the spawn family; core's `run_turn` still filters it through the #116
/// tool mask, so the profile's `tools:` allowlist must also list `propose_plan`.
pub fn specs_for(profile: &AgentProfile) -> Vec<ToolSpec> {
    if profile.owns_plan {
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
/// The caller subscribes to the inbound fan-out *before* handing off (so a fast
/// decision can't race ahead) and passes the receiver in — mirroring the approval
/// path in [`crate::tool_runner`]. A `Stop` while parked unwinds silently: core's
/// turn cancels on the same `Stop`, so no `ToolResult` is owed.
pub async fn run_propose_plan(
    holly: Holly,
    mut inbound: Receiver<InMsg>,
    session: SessionId,
    seq: u64,
    request_id: String,
    input: String,
) {
    let plan = parse_plan(&input);

    // A standard `ToolRequest` — the head renders the usual approve/reject prompt.
    let _ = holly.events().send(OutEvent::ToolRequest {
        session: session.clone(),
        seq,
        request_id: request_id.clone(),
        tool: PROPOSE_PLAN_TOOL.to_string(),
        input,
    });
    let _ = holly.events().send(OutEvent::Status {
        session: session.clone(),
        state: AgentState::WaitingApproval,
    });

    loop {
        match inbound.recv().await {
            Ok(InMsg::Approve {
                session: s,
                request_id: rid,
            }) if s == session && rid == request_id => {
                set_thinking(&holly, &session);
                // Record the plan into engine state so every head sees it, then
                // tell the model the plan was accepted. The head performs the
                // fresh-session handoff (ADR-0042) — no new protocol surface.
                let _ = holly
                    .send(InMsg::SetPlan {
                        session: session.clone(),
                        content: plan,
                    })
                    .await;
                reply(
                    &holly,
                    session,
                    request_id,
                    "plan accepted by the user".to_string(),
                )
                .await;
                return;
            }
            Ok(InMsg::Reject {
                session: s,
                request_id: rid,
                reason,
            }) if s == session && rid == request_id => {
                set_thinking(&holly, &session);
                let output = format!(
                    "tool `{PROPOSE_PLAN_TOOL}` rejected: {}",
                    reason.as_deref().unwrap_or("user")
                );
                reply(&holly, session, request_id, output).await;
                return;
            }
            Ok(InMsg::Stop { session: s }) if s == session => return,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

async fn reply(holly: &Holly, session: SessionId, request_id: String, output: String) {
    let _ = holly
        .send(InMsg::ToolResult {
            session,
            request_id,
            output,
        })
        .await;
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
    fn specs_advertised_only_to_owns_plan_profiles() {
        use entanglement_core::ProfileRegistry;
        let reg = ProfileRegistry::new(); // build/explore: !owns_plan, plan: owns_plan
        assert!(
            specs_for(reg.get("build").unwrap()).is_empty(),
            "a non-owning profile gets no propose_plan spec"
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
