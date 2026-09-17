//! `propose_plan` — the plan agent's one tool: submit a plan (`content` XOR
//! `path`) for the user's approval (#141, ADR-0042; #513, ADR-0145; #560,
//! ADR-0207 §7, which retires the sponsored-build handoff below in favor of
//! a plain mode switch).
//!
//! A plan is a **file** under `.entanglement/plans/`, not an in-memory
//! snapshot: `content` materializes (or overwrites) one there and `path` binds
//! to an existing `.md` file the agent already wrote or the user seeded (parse/
//! resolve logic lives in the sibling `resolve` module, split out to stay
//! under the file-size cap). Either way the resolved content rides back as an
//! `OutEvent::Plan` snapshot for the plan session itself (the wire event
//! non-TUI heads render) and the subsequent `ToolRequest` — the tool is
//! intercepted on [`OutEvent::ToolExec`] — like `ask_user` (ADR-0027) — and
//! **force-parked on the `Ask` path unconditionally** once past the mode
//! grade below. A malformed call (both/neither of `content`/`path`, a
//! missing/non-`.md` `path`, or a stale `path` — see the staleness guard
//! below) is refused immediately instead, with no approval prompt: it is a
//! self-correctable model error, not a decision for the human.
//!
//! **Plan authorship is graded by capability, not advertisement** (ADR-0207
//! §7): `propose_plan` carries `Capability::Plan`
//! ([`crate::capability::runtime_owned`]) and is advertised unconditionally —
//! the mode grade below is what actually closes authorship outside `plan`
//! mode. A mode that denies `Plan` (`research`/`build`/`auto`, per the
//! built-in table) declines the call flat, naming the mode and the way out,
//! *before* any file is materialized or an approval is ever parked. A mode
//! that allows it (`Allow` or `Ask` — `plan` mode grades it `Allow`) still
//! goes through the unconditional force-park below: grading only decides
//! whether the model may ask at all, never whether asking is skipped — user
//! approval *is* the tool's semantics, and no mode may `Allow` past it.
//!
//! **Staleness guard** (`path` mode only): the session must be the last party
//! known to have touched the bound file — tracked by [`crate::plan_files`] off
//! this module's own reads/writes plus the executor's `FileChange` audit for a
//! matching in-session `edit`/`write`/`apply_patch`. A file the *user* edited
//! out of band since is refused with a re-read-required error.
//!
//! - **Approve** → the session (and its live spawn sub-tree — core cascades
//!   `InMsg::SetMode` over it, ADR-0207 §6) switches to `build` mode and the
//!   call returns immediately, naming the plan file. No sponsored child, no
//!   permission root, no blocking wait: the same turn continues, plan still
//!   in context, and the model that wrote it executes it directly. This
//!   supersedes ADR-0138's sponsored `build` handoff entirely — approving a
//!   plan today spawns nothing.
//! - **Reject + reason** → the existing rejection fold-back (`tool
//!   \`propose_plan\` rejected: <reason>`); the model revises and re-proposes in
//!   the same turn, no new code.
//!
//! **Stop while parked on the Ask wait**: registered with
//! [`crate::cancel::CancelRegistry`] by its caller (`tool_runner`'s
//! `Intercept::ProposePlan` arm), so a `Stop` targeting the plan session
//! aborts the wait — core's own turn cancellation on the same `Stop` already
//! means no `ToolResult` is owed.

use std::path::PathBuf;
use std::sync::Arc;

use entanglement_core::{AgentState, Holly, InMsg, OutEvent, Permission, SessionId, ToolSpec};

use crate::pending::{self, PendingDecisions};
use crate::plan_files::PlanFileRegistry;
use crate::policy::PermissionResolver;
use crate::seam;
use crate::tool_names::PROPOSE_PLAN_TOOL;
use crate::tool_runner::resolve_effective;

mod resolve;
pub(crate) use resolve::PLANS_DIR;
use resolve::{parse_plan_input, resolve_plan};

/// The mode an approved plan's session (and its live spawn sub-tree) switches
/// to (ADR-0207 §7) — the same name the built-in mode table calls its
/// ordinary implementation posture.
const BUILD_MODE: &str = "build";

/// The `propose_plan` tool schema. Advertised **unconditionally** now
/// (ADR-0207 §7 — the old default-closed, per-profile allowlist gate is
/// retired along with the mask it read): every session sees it, and the mode
/// grade below is the gate that actually decides authorship. Rides the
/// shared `tool_specs`, like `update_tasks`.
pub fn propose_plan_spec() -> ToolSpec {
    ToolSpec::with_schema(
        PROPOSE_PLAN_TOOL,
        "Submit the plan for the user's acceptance. Provide exactly one of \
         `content` (plan markdown to materialize as a new plan file under \
         .entanglement/plans/) or `path` (an existing .md plan file to submit \
         as-is — it must be the file you most recently read, wrote, or edited; \
         a file changed by someone else since is refused, re-read it first). \
         Only usable in `plan` mode — request it with request_mode if you are \
         not there yet. The user approves or rejects: on approval this \
         session's mode switches to `build` and you continue the same turn \
         implementing the plan directly. On rejection you receive their \
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

/// Orchestrate one `propose_plan` call: grade plan authorship for the
/// session's current mode, resolve its `content`/`path` input to a file,
/// surface it as a standard approval prompt, and park for the head's
/// decision.
///
/// A mode `Deny` (ADR-0207 §7) or a resolution failure (bad input, missing
/// file, staleness) replies immediately with no `ToolRequest` ever emitted —
/// never registers a pending waiter for it, so nothing is left dangling.
/// Otherwise registers the waiter with the lag-proof [`PendingDecisions`]
/// registry (#156) *before* emitting the request, so a fast decision routes
/// to this park rather than racing a per-task broadcast subscription that
/// could lag and drop it. A `Stop` while parked unwinds silently: core's turn
/// cancels on the same `Stop`, so no `ToolResult` is owed.
///
/// On **Approve**, switches `session` to `build` mode (`InMsg::SetMode` —
/// core cascades this over the session's live spawn sub-tree, ADR-0207 §6)
/// and replies at once; the same turn continues with the plan already in
/// context.
#[allow(clippy::too_many_arguments)]
pub async fn run_propose_plan(
    holly: Holly,
    pending: PendingDecisions,
    resolver: Arc<dyn PermissionResolver>,
    chain: Vec<SessionId>,
    mode: String,
    plan_files: Arc<PlanFileRegistry>,
    root: PathBuf,
    session: SessionId,
    request_id: String,
    input: String,
) {
    // ADR-0207 §7: graded by capability before anything else runs — a mode
    // that denies `Plan` never even gets a materialized file or a parked
    // approval out of a call it was always going to refuse.
    let perm = resolve_effective(&*resolver, &chain, PROPOSE_PLAN_TOOL, &input).await;
    if perm == Permission::Deny {
        let output =
            format!("tool `{PROPOSE_PLAN_TOOL}` denied by mode `{mode}` — use /mode to switch");
        seam::reply(&holly, session, request_id, output, true).await;
        return;
    }

    let plan_input = match parse_plan_input(&input) {
        Ok(p) => p,
        Err(msg) => {
            seam::reply(&holly, session, request_id, msg, true).await;
            return;
        }
    };
    let resolution = match resolve_plan(&root, &session, &plan_files, plan_input) {
        Ok(r) => r,
        Err(msg) => {
            seam::reply(&holly, session, request_id, msg, true).await;
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
    let rx = pending.register(&session, &request_id, "plan", resolution.rel_path.clone());

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
            // ADR-0207 §7: approval is a mode switch, not a spawn. Core
            // cascades this `SetMode` over the session's whole live spawn
            // sub-tree (ADR-0207 §6, `holly.rs`'s `InMsg::SetMode` handling),
            // so a plan session with running children switches them too.
            if holly
                .send(InMsg::SetMode {
                    session: session.clone(),
                    mode: BUILD_MODE.to_string(),
                })
                .await
                .is_err()
            {
                // Engine inbox closed — nothing left to reply to either.
                return;
            }
            set_thinking(&holly, &session);
            let output = format!(
                "plan file: {}\n\nplan approved — this session's mode switched to \
                 `{BUILD_MODE}`. Continue the same turn, implementing the plan directly.",
                resolution.rel_path
            );
            seam::reply(&holly, session, request_id, output, false).await;
        }
        seam::Decision::Reject { reason } => {
            set_thinking(&holly, &session);
            let output = format!(
                "tool `{PROPOSE_PLAN_TOOL}` rejected (plan file: {}): {}",
                resolution.rel_path,
                reason.as_deref().unwrap_or("user")
            );
            seam::reply(&holly, session, request_id, output, true).await;
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

fn set_thinking(holly: &Holly, session: &SessionId) {
    holly.emit_status(session, AgentState::Thinking);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_no_required_array_since_content_and_path_are_mutually_exclusive() {
        let spec = propose_plan_spec();
        assert_eq!(spec.name, PROPOSE_PLAN_TOOL);
        assert!(spec.schema.get("required").is_none());
        assert!(spec.schema["properties"].get("content").is_some());
        assert!(spec.schema["properties"].get("path").is_some());
    }
}
