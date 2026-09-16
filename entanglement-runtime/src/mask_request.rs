//! Out-of-mask tool calls as an approval round-trip (ADR-0198).
//!
//! Before this ADR, a call to a tool outside the session's effective tool
//! mask (#116/ADR-0038, ADR-0149's overlay) died with an attributed flat
//! decline ([`crate::decline::mask_decline`]) — the model could see the
//! tool's schema (advertisement is universal, ADR-0192) but never run it.
//! Now the mask miss instead parks a normal approval: the user approving it
//! unlocks the call, `Once` for just this one or `Session` for the rest of
//! the session (materialized as an ADR-0149 overlay **enable** entry, so
//! later calls pass the mask on their own).
//!
//! Three hard limits still flat-decline outright, no prompt ever offered —
//! [`is_spawn_tool`] and [`explicit_deny_floor`] are the pure checks the
//! executor's dispatch loop runs *before* routing here; a fourth (an unknown
//! tool name) is checked by the loop directly against the registry, since it
//! needs no helper. Everything else lands in [`handle`], which parks the
//! approval and, once granted, replays [`crate::tool_runner::dispatch`] —
//! the *same* function an in-mask call's `Ask` grade goes through — so a
//! `Once` approval whose underlying permission grade is itself `Ask` may
//! legitimately ask again (the mask was not the only reason to prompt); a
//! `Session` approval never does, since the overlay entry it writes first
//! forces the replayed grade to `Allow`. See ADR-0198 for the full
//! single-prompt rationale and the scopes this module does and does not
//! offer (no `Always`; `SessionDir` degrades to `Session`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use entanglement_core::{
    AgentProfile, AgentState, ApprovalScope, Holly, InMsg, OutEvent, PermissionProfile, SessionId,
    ToolOverlayEntry,
};

use crate::arg_validate::LoopBreaker;
use crate::decline::{mask_request_attribution, MaskSource};
use crate::hooks::Hooks;
use crate::mcp::{ActiveServers, AvailableMcp};
use crate::pending::PendingDecisions;
use crate::policy::{GrantStore, PermissionResolver};
use crate::seam;
use crate::skills::SkillRegistry;
use crate::tool_advertising::AdvertisingState;
use crate::tool_names::{AGENT_SEND_TOOL, AGENT_TOOL};
use crate::tool_runner::{dispatch, set_thinking, EscapeRoot};
use crate::tools::{SharedRegistry, ToolRegistry};

/// Hard limit (b), ADR-0198 §2: `agent`/`agent_send` are profile-defining
/// (the ADR-0192 spawn-enum carve-out — sponsorship, ADR-0138, depends on
/// spawn staying a hard per-profile boundary), so an out-of-mask call to
/// either keeps the pre-ADR-0198 flat decline, never an approval offer.
pub(crate) fn is_spawn_tool(tool: &str) -> bool {
    tool == AGENT_TOOL || tool == AGENT_SEND_TOOL
}

/// Hard limit (a), ADR-0198 §2: an **explicit, bare-name** `Deny` rule in
/// the active profile chain or the config ceiling — the profile author's
/// (or the operator's) deliberate "never" for this specific tool. Only a
/// literal `tool` key counts, not the ambient default every tool a profile
/// never mentions falls through to (`explore`'s `default: deny` does not
/// float `edit` here — nothing in its rule set ever names `edit`) and not a
/// `*`/`tool(pattern)`/`tool{pattern}` rule (see
/// [`PermissionProfile::explicit_bare_deny`]).
///
/// Deliberately reads the local `AgentProfile`/config-ceiling rule sets
/// directly rather than the pluggable [`PermissionResolver`] seam: the mask
/// check itself already does this (`crate::permission::tool_mask_source`
/// reads `active` directly), and the resolver's `Permission` return value is
/// opaque — a custom embedder resolver's own `Deny` can't be told apart from
/// an ambient default, so it is never treated as this hard floor. An
/// embedder wanting its own resolver's `Deny` to also block the approval
/// offer would need to express that as an explicit bare rule on the
/// session's `AgentProfile` (documented in ADR-0198's consequences).
pub(crate) fn explicit_deny_floor(
    active: &HashMap<SessionId, AgentProfile>,
    chain: &[SessionId],
    ceiling: &PermissionProfile,
    tool: &str,
) -> bool {
    ceiling.explicit_bare_deny(tool)
        || chain.iter().any(|s| {
            active
                .get(s)
                .is_some_and(|p| p.permission.explicit_bare_deny(tool))
        })
}

/// Park a mask-attributed approval for a mask-miss call that cleared both
/// hard limits, then run-or-refuse per the head's decision. Mirrors
/// [`crate::tool_runner::dispatch`]'s own `Ask` branch (registers with
/// [`PendingDecisions`] before emitting, mints a fresh per-session seq,
/// flips the status to `WaitingApproval`) so the wire shape a head renders
/// is identical to any other approval prompt — only the `input` text's
/// appended attribution (no protocol change, ADR-0198 §3) marks it as a
/// mask offer rather than a permission one.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle(
    holly: &Holly,
    tools: &ToolRegistry,
    skills: &Arc<RwLock<Arc<SkillRegistry>>>,
    active_skill: &Arc<Mutex<HashSet<SessionId>>>,
    resolver: &dyn PermissionResolver,
    chain: &[SessionId],
    grants: &dyn GrantStore,
    hooks: &Hooks,
    pending: &PendingDecisions,
    escape_root: Option<&EscapeRoot>,
    // The chain's existing overlay grade entry, if any — resolved by the
    // caller exactly as the ordinary `Intercept::Permission` route resolves
    // it (`crate::permission::overlay_grade_entry`), so a `Once` approval
    // replays `dispatch` with the identical grade-override input an in-mask
    // call would have had.
    overlay_entry: Option<ToolOverlayEntry>,
    // The session's own current overlay entries, for computing the
    // full-replacement `SetToolOverlay` a `Session` approval sends — mirrors
    // the TUI's own `/enable tool` writer (`tui/enable_command.rs`).
    own_overlay: Vec<ToolOverlayEntry>,
    ceiling: &PermissionProfile,
    advertising: &AdvertisingState,
    validation: &LoopBreaker,
    // ADR-0201's dispatch-time lazy MCP re-enable — forwarded verbatim into
    // the `dispatch` call below so an approved out-of-mask `mcp__<server>__*`
    // call self-heals exactly like the ordinary in-mask route.
    registry: &SharedRegistry,
    mcp_avail: &AvailableMcp,
    mcp_active: &ActiveServers,
    http: Option<&entanglement_core::HttpClient>,
    source: MaskSource,
    agent_name: Option<String>,
    session: SessionId,
    request_id: String,
    tool: String,
    input: String,
) {
    let rx = pending.register(&session, &request_id);
    let attribution = mask_request_attribution(&source, &session, agent_name.as_deref(), &tool);
    holly.emit_for_session(&session, |seq| OutEvent::ToolRequest {
        session: session.clone(),
        seq,
        request_id: request_id.clone(),
        tool: tool.clone(),
        input: format!("{input}\n\n⚠ {attribution} — approve to run it"),
    });
    holly.emit_status(&session, AgentState::WaitingApproval);

    match crate::pending::await_decision(rx).await {
        seam::Decision::Approve { scope } => {
            set_thinking(holly, &session);
            let forced_entry = match scope {
                // Nothing persists — replay `dispatch` with the mask's
                // pre-existing overlay grade (if any), unchanged. If the
                // underlying permission grade is itself `Ask`, dispatch may
                // legitimately ask again (ADR-0198's documented trade-off);
                // if it is `Allow`, this was the only prompt.
                ApprovalScope::Once => overlay_entry,
                // `SessionDir` has no narrower meaning for a whole-tool mask
                // widening, so it degrades to `Session` (ADR-0198 §1).
                ApprovalScope::Session | ApprovalScope::SessionDir => {
                    Some(materialize_session_grant(holly, &session, &own_overlay, &tool).await)
                }
                // ADR-0198 offers no durable mask-widening scope (durable
                // widening stays an explicit agent-file/allowlist edit,
                // ADR-0083) — the protocol's approval scope set is fixed
                // (ADR-0052), so degrade rather than reject the approval.
                ApprovalScope::Always => {
                    tracing::warn!(
                        %session,
                        tool = %tool,
                        "ADR-0198: no durable mask-widening scope — an Always approval on an \
                         out-of-mask tool degrades to Session"
                    );
                    Some(materialize_session_grant(holly, &session, &own_overlay, &tool).await)
                }
            };
            dispatch(
                holly,
                tools,
                skills,
                active_skill,
                resolver,
                chain,
                grants,
                hooks,
                pending,
                escape_root,
                forced_entry,
                ceiling,
                advertising,
                validation,
                registry,
                mcp_avail,
                mcp_active,
                http,
                session,
                request_id,
                tool,
                input,
            )
            .await;
        }
        seam::Decision::Reject { reason } => {
            set_thinking(holly, &session);
            let output = format!(
                "tool `{tool}` rejected: {}",
                reason.as_deref().unwrap_or("user")
            );
            seam::reply(holly, session, request_id, output, true).await;
        }
        // `Stop` (and a closed inbox) unwind silently, matching
        // `tool_runner::await_decision`; the remaining variants never target
        // a tool-approval request id.
        seam::Decision::Stop
        | seam::Decision::Answer { .. }
        | seam::Decision::Retract
        | seam::Decision::Replace { .. } => {}
    }
}

/// Materialize a durable ADR-0149 overlay **enable** entry that grants
/// outright (`ToolOverlayEntry::allow`, no `arg_pattern`) — so `tool` both
/// exists for the rest of `session` *and* skips the permission prompt on
/// every later call, matching the "stop asking me" semantics an ordinary
/// Session grant already gives an in-mask `Ask` tool (ADR-0052). Sent as a
/// full-replacement `SetToolOverlay` — the same trusted-only, privileged
/// `holly.send` path the TUI's own `/enable tool` command uses — upserting
/// over whatever `own_overlay` last held (a same-pattern entry, if any, is
/// replaced rather than duplicated). Returns the entry so the caller can
/// hand the *exact same* value straight to `dispatch` without waiting for
/// the `ToolOverlayChanged` fold to round-trip back to this executor's own
/// loop, closing the race a re-read would otherwise have.
async fn materialize_session_grant(
    holly: &Holly,
    session: &SessionId,
    own_overlay: &[ToolOverlayEntry],
    tool: &str,
) -> ToolOverlayEntry {
    let mut entries: Vec<ToolOverlayEntry> = own_overlay
        .iter()
        .filter(|e| e.pattern != tool)
        .cloned()
        .collect();
    let entry = ToolOverlayEntry::allow(tool);
    entries.push(entry.clone());
    let _ = holly
        .send(InMsg::SetToolOverlay {
            session: session.clone(),
            entries,
        })
        .await;
    entry
}

#[cfg(test)]
mod tests {
    use entanglement_core::{AgentMode, Permission};

    use super::*;

    fn profile(name: &str, permission: PermissionProfile) -> AgentProfile {
        AgentProfile {
            name: name.into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            model: None,
            provider: None,
            permission,
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        }
    }

    #[test]
    fn spawn_tools_are_the_only_hard_limit_b() {
        assert!(is_spawn_tool(AGENT_TOOL));
        assert!(is_spawn_tool(AGENT_SEND_TOOL));
        assert!(!is_spawn_tool("edit"));
        assert!(!is_spawn_tool("bash"));
    }

    #[test]
    fn ambient_default_deny_is_not_a_floor() {
        // `explore`'s exact shape: `default: deny`, no rule ever names `edit`.
        let mut active = HashMap::new();
        active.insert(
            SessionId::new("s1"),
            profile("explore", PermissionProfile::new(Permission::Deny)),
        );
        let ceiling = PermissionProfile::new(Permission::Allow);
        assert!(!explicit_deny_floor(
            &active,
            &[SessionId::new("s1")],
            &ceiling,
            "edit"
        ));
    }

    #[test]
    fn an_explicit_bare_rule_floors_regardless_of_which_link_carries_it() {
        let mut active = HashMap::new();
        active.insert(
            SessionId::new("s1"),
            profile(
                "locked",
                PermissionProfile::new(Permission::Allow).with("bash", Permission::Deny),
            ),
        );
        let ceiling = PermissionProfile::new(Permission::Allow);
        assert!(explicit_deny_floor(
            &active,
            &[SessionId::new("s1")],
            &ceiling,
            "bash"
        ));
        // An ancestor's explicit rule floors the child too.
        active.insert(
            SessionId::new("child"),
            profile("open", PermissionProfile::new(Permission::Allow)),
        );
        assert!(explicit_deny_floor(
            &active,
            &[SessionId::new("child"), SessionId::new("s1")],
            &ceiling,
            "bash"
        ));
    }

    #[test]
    fn the_config_ceiling_can_floor_on_its_own() {
        let active = HashMap::new();
        let ceiling = PermissionProfile::new(Permission::Allow).with("bash", Permission::Deny);
        assert!(explicit_deny_floor(&active, &[], &ceiling, "bash"));
    }
}
