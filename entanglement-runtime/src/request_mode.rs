//! `request_mode` — a blocked model's escape hatch out of a mode that denies
//! it a tool it needs (#560, ADR-0207 §10).
//!
//! `Capability::Control` ([`crate::capability::runtime_owned`]), so it is
//! never graded by the session's permission mode — the capability bypass
//! `tool_runner::dispatch` already gives every Control tool (§3) applies here
//! automatically, with nothing to add for grading. What *is* special-cased,
//! narrowly, inside that same bypass branch is orchestration: unlike
//! `update_tasks`/`load_skill`/`mcp_enable` (which just run and reply),
//! `request_mode` force-parks an approval exactly like `propose_plan` —
//! asking to widen a session's authority is a decision only the user makes,
//! never something a mode grade can wave through. It has no `Intercept`
//! route of its own; it reaches this module's [`run_request_mode`] by tool
//! name, from inside `dispatch`'s Control branch.
//!
//! Four refusal shapes, checked in order, each replying immediately with no
//! approval ever parked:
//!
//! 1. **Malformed input** — no `mode` field. Self-correctable, not a
//!    decision for the human (mirrors `propose_plan`'s validation-error
//!    replies).
//! 2. **Current mode is `auto`** — refused outright, unconditionally,
//!    regardless of the requested target (ADR-0207 §11): an unattended run
//!    has no one to approve the escalation, so the request can't even reach
//!    a park. This is the property that makes `auto` safe to leave running.
//! 3. **Target is `auto`** — refused from *any* current mode: a model asking
//!    to switch off supervision is not a request anyone should be one
//!    keystroke from granting.
//! 4. **Not a widening pair** (only `research`→`plan`, `research`→`build`,
//!    `plan`→`build` widen) — refused; narrowing is the user's own action
//!    (`/mode`), never something the model requests on its own behalf.
//!
//! Past all four, the call force-parks on `Ask` unconditionally — approval
//! *is* the tool's semantics, mirroring `propose_plan`. On approval it
//! applies the switch the same way `InMsg::SetMode` always has (core
//! cascades it over the session's live spawn sub-tree, ADR-0207 §6) and
//! replies at once; on rejection it folds the typed reason back.

use entanglement_core::{AgentState, Holly, InMsg, OutEvent, SessionId};

use crate::pending::{self, PendingDecisions};
use crate::seam;
use crate::tool_names::REQUEST_MODE_TOOL;

/// The one mode `request_mode` may never target, from any current mode
/// (ADR-0207 §10) — an unattended run cannot escalate its own authority, so
/// nothing may ask its way into the one mode with no one watching to approve.
const AUTO_MODE: &str = "auto";

/// Every mode pair `request_mode` may widen across (ADR-0207 §10) — anything
/// not listed here (narrowing, a same-mode request, an unknown mode name, or
/// any pair targeting [`AUTO_MODE`]) is refused as non-widening.
const WIDENING_PAIRS: &[(&str, &str)] = &[
    ("research", "plan"),
    ("research", "build"),
    ("plan", "build"),
];

fn widens(from: &str, to: &str) -> bool {
    WIDENING_PAIRS.contains(&(from, to))
}

/// The `request_mode` tool schema. Advertised **unconditionally** (ADR-0207
/// §10), riding [`crate::discover::runtime_owned_specs`] and the
/// `tool_search` kernel like `propose_plan` — a blocked model must always be
/// able to name what it needs, in every mode including the one that would
/// deny the ask itself. The `enum` names only the two real widening targets;
/// `run_request_mode` still validates at the widening-pair level regardless,
/// since a raw/scripted backend can send anything.
pub fn request_mode_spec() -> entanglement_core::ToolSpec {
    entanglement_core::ToolSpec::with_schema(
        REQUEST_MODE_TOOL,
        "Ask the user to widen this session's permission mode when a tool you \
         need is blocked. Only widens: research -> plan, research -> build, \
         plan -> build. Never targets auto, and is refused outright while \
         already in auto mode — an unattended run cannot grant itself more \
         authority. The user approves or rejects; to narrow the mode instead, \
         that's the user's own action via /mode, not something to request.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["plan", "build"],
                    "description": "The mode to request switching to."
                },
                "reason": {
                    "type": "string",
                    "description": "Why this mode is needed right now."
                }
            },
            "required": ["mode", "reason"]
        }),
    )
}

/// Parse `request_mode`'s `{"mode": ..., "reason": ...}` input. `reason`
/// rides along for the approval prompt's context but isn't itself validated —
/// an absent one just renders as an empty explanation.
fn parse_target_mode(input: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Orchestrate one `request_mode` call — see the module doc for the four
/// refusal shapes and the force-park past them. `current_mode` is the
/// session's own mode as `tool_runner::dispatch` already resolved it (no
/// ancestor chain to consult: `Control` is never graded, so there is no
/// clamp here to reuse).
pub async fn run_request_mode(
    holly: &Holly,
    pending: &PendingDecisions,
    session: SessionId,
    request_id: String,
    input: String,
    current_mode: String,
) {
    let Some(requested) = parse_target_mode(&input) else {
        let output =
            "request_mode requires a `mode` field naming the mode to switch to".to_string();
        seam::reply(holly, session, request_id, output, true).await;
        return;
    };

    // 2. `auto` refuses every request outright — no park, regardless of
    // target (ADR-0207 §11: nobody is present to answer the prompt).
    if current_mode == AUTO_MODE {
        let output = format!(
            "tool `{REQUEST_MODE_TOOL}` refused: an unattended `auto` run cannot request more \
             authority than it was started with"
        );
        seam::reply(holly, session, request_id, output, true).await;
        return;
    }
    // 3. `auto` is never a valid target, from any current mode.
    if requested == AUTO_MODE {
        let output = format!(
            "tool `{REQUEST_MODE_TOOL}` refused: `auto` can never be requested — switching into \
             an unattended posture is the user's own action (/mode), not something a session \
             asks for itself"
        );
        seam::reply(holly, session, request_id, output, true).await;
        return;
    }
    // 4. Only a widening pair may be requested; narrowing (or a same-mode /
    // unknown-mode request) is refused the same way.
    if !widens(&current_mode, &requested) {
        let output = format!(
            "tool `{REQUEST_MODE_TOOL}` refused: `{current_mode}` -> `{requested}` does not \
             widen authority — narrowing (or switching sideways) is the user's own action \
             (/mode), never something to request"
        );
        seam::reply(holly, session, request_id, output, true).await;
        return;
    }

    // Past every refusal: force-park, always — approval is this tool's own
    // semantics, exactly like `propose_plan`.
    let rx = pending.register(&session, &request_id, "mode", requested.clone());
    holly.emit_for_session(&session, |seq| OutEvent::ToolRequest {
        session: session.clone(),
        seq,
        request_id: request_id.clone(),
        tool: REQUEST_MODE_TOOL.to_string(),
        input: input.clone(),
    });
    holly.emit_status(&session, AgentState::WaitingApproval);

    match pending::await_decision(rx).await {
        seam::Decision::Approve { .. } => {
            // Applies the switch exactly like `InMsg::SetMode` always has —
            // core cascades it over the session's live spawn sub-tree
            // (ADR-0207 §6).
            if holly
                .send(InMsg::SetMode {
                    session: session.clone(),
                    mode: requested.clone(),
                })
                .await
                .is_err()
            {
                // Engine inbox closed — nothing left to reply to either.
                return;
            }
            holly.emit_status(&session, AgentState::Thinking);
            let output =
                format!("mode request approved — this session's mode switched to `{requested}`.");
            seam::reply(holly, session, request_id, output, false).await;
        }
        seam::Decision::Reject { reason } => {
            holly.emit_status(&session, AgentState::Thinking);
            let output = format!(
                "tool `{REQUEST_MODE_TOOL}` rejected: {}",
                reason.as_deref().unwrap_or("user")
            );
            seam::reply(holly, session, request_id, output, true).await;
        }
        // `Stop` (and a closed inbox) unwind silently; `Answer`/`Retract`/
        // `Replace` never target a `request_mode` request id (they are
        // `ask_user`-only, #515).
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_names_the_two_real_widening_targets() {
        let spec = request_mode_spec();
        assert_eq!(spec.name, REQUEST_MODE_TOOL);
        let enum_vals = spec.schema["properties"]["mode"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(enum_vals, &["plan", "build"]);
    }

    #[test]
    fn parse_target_mode_reads_the_mode_field() {
        assert_eq!(
            parse_target_mode(r#"{"mode":"build","reason":"need to write"}"#).as_deref(),
            Some("build")
        );
        assert_eq!(parse_target_mode(r#"{"reason":"x"}"#), None);
        assert_eq!(parse_target_mode("not json"), None);
    }

    #[test]
    fn widening_table_allows_exactly_the_three_documented_pairs() {
        assert!(widens("research", "plan"));
        assert!(widens("research", "build"));
        assert!(widens("plan", "build"));
        // Everything else — narrowing, same-mode, sideways, or unknown —
        // does not widen.
        assert!(!widens("build", "plan"));
        assert!(!widens("plan", "research"));
        assert!(!widens("build", "research"));
        assert!(!widens("build", "build"));
        assert!(!widens("research", "research"));
        assert!(!widens("research", "auto"));
        assert!(!widens("bogus", "build"));
    }
}
