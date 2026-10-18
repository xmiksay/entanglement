//! Parking a tool call for approval, and listing what is parked (#726).
//!
//! Every approval path (permission `Ask`, `propose_plan`, `request_mode`, a
//! `rhai` binding's `Ask`) parks through [`park_approval`], which registers
//! the waiter *and* emits the [`OutEvent::ToolRequest`] from one
//! [`PendingApproval`] value. WHY one value: `InMsg::ListPending` must report
//! exactly the prompt a reconnecting head missed — building the event and the
//! registry entry from the same struct means they can't drift.

use std::time::{SystemTime, UNIX_EPOCH};

use entanglement_core::{
    ApprovalKind, ApprovalScope, CardReason, Holly, OutEvent, PendingApproval, SessionId,
};
use tokio::sync::oneshot;

use super::PendingDecisions;
use crate::agent_registry::AgentRegistry;
use crate::seam::Decision;

/// The scopes an `Approve` on a `kind`/`tool` park actually honors — what a
/// head should offer. `propose_plan`/`request_mode` ignore the scope (their
/// approver picks a *mode*, not a grant lifetime), so only `Once`; so is a
/// floored write (it always asks, so no grant is recorded) and an inline
/// `rhai` launch (no path to key a grant on). `SessionDir` widens only the
/// read-only triad (ADR-0126), and not on an escape card — elsewhere it
/// would silently degrade to `Session`, so it isn't offered. `agent_send`
/// grades on a handle minted for this run (ADR-0235), so a persisted
/// `Always` grant could never match again — not offered either.
pub fn scopes_for(kind: ApprovalKind, tool: &str, card: Option<CardReason>) -> Vec<ApprovalScope> {
    let once_only = matches!(
        card,
        Some(CardReason::FlooredWrite | CardReason::InlineLaunch)
    );
    match kind {
        ApprovalKind::Plan | ApprovalKind::Mode => vec![ApprovalScope::Once],
        ApprovalKind::Tool | ApprovalKind::Rhai if once_only => vec![ApprovalScope::Once],
        ApprovalKind::Tool if tool == crate::tool_names::AGENT_SEND_TOOL => {
            vec![ApprovalScope::Once, ApprovalScope::Session]
        }
        ApprovalKind::Tool | ApprovalKind::Rhai => {
            let mut scopes = vec![
                ApprovalScope::Once,
                ApprovalScope::Session,
                ApprovalScope::Always,
            ];
            // An escape grant is keyed on the exact path (`ExtraRootStore`
            // folds `SessionDir` into `Session`), so a directory grant would
            // over-promise there.
            if crate::tool_names::is_read_capability_member(tool)
                && card != Some(CardReason::RootEscape)
            {
                scopes.push(ApprovalScope::SessionDir);
            }
            scopes
        }
    }
}

/// Whether a head with nobody to ask (`skutter run`) may start this card:
/// only a `rhai` launch inside the root, which then runs Ask per command —
/// every nested call is still graded. Decided on the structured reason, so
/// an out-of-root launch ([`CardReason::LaunchEscape`]) — or any card
/// without a launch reason — is refused (ADR-0230).
pub fn headless_starts(card: Option<CardReason>) -> bool {
    matches!(
        card,
        Some(CardReason::ScriptLaunch | CardReason::InlineLaunch)
    )
}

/// Build the [`PendingApproval`] for a call about to park, stamping its
/// scope options and park time. `card` is the structured reason a head
/// decides on (ADR-0230); it also narrows the scopes.
pub fn approval(
    session: &SessionId,
    request_id: &str,
    kind: ApprovalKind,
    tool: impl Into<String>,
    input: impl Into<String>,
    reason: impl Into<String>,
    card: Option<CardReason>,
) -> PendingApproval {
    let tool = tool.into();
    PendingApproval {
        session: session.clone(),
        request_id: request_id.to_string(),
        kind,
        scopes: scopes_for(kind, &tool, card),
        card_reason: card,
        tool,
        input: input.into(),
        reason: reason.into(),
        // A pre-epoch clock is a broken host, not a reason to fail a park.
        since_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default(),
    }
}

/// Register `approval`'s waiter, then emit its `ToolRequest` (register-then-
/// emit, #156, so the router can never see the decision before the waiter).
/// `detail` is the short label both pending listings show — a head's
/// `InMsg::ListPending` and the subtree section of a handle-less `poll`
/// (#829). The prompt mints a fresh per-session seq (#157).
pub fn park_approval(
    holly: &Holly,
    pending: &PendingDecisions,
    approval: PendingApproval,
    detail: impl Into<String>,
) -> oneshot::Receiver<Decision> {
    park_approval_as(holly, pending, approval, detail, false)
}

/// [`park_approval`], marking the park `detached` for a background script's
/// nested call — it survives its session's end or hibernate
/// ([`PendingDecisions::stop_attached`]).
pub fn park_approval_as(
    holly: &Holly,
    pending: &PendingDecisions,
    approval: PendingApproval,
    detail: impl Into<String>,
    detached: bool,
) -> oneshot::Receiver<Decision> {
    let session = approval.session.clone();
    let event = |seq| OutEvent::ToolRequest {
        session: approval.session.clone(),
        seq,
        request_id: approval.request_id.clone(),
        tool: approval.tool.clone(),
        input: approval.input.clone(),
        reason: approval.card_reason,
        scopes: approval.scopes.clone(),
    };
    let rx = pending.register_approval(approval.clone(), detail, detached);
    holly.emit_for_session(&session, event);
    rx
}

/// The `InMsg::ListPending` answer (#726): every live parked approval, or —
/// with `root` — only those in `root`'s live spawn sub-tree (the same walk
/// [`crate::discover::subtree_sessions`] walk).
pub fn list_pending(
    pending: &PendingDecisions,
    agents: &AgentRegistry,
    root: Option<&SessionId>,
) -> Vec<PendingApproval> {
    match root {
        None => pending.approvals(|_| true),
        Some(root) => {
            let subtree = crate::discover::subtree_sessions(agents, root);
            pending.approvals(|s| subtree.contains(s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0235: an `agent_send` grant is keyed on a minted handle, so it
    /// is never offered as a persisted `Always`.
    #[test]
    fn agent_send_offers_no_always_scope() {
        assert_eq!(
            scopes_for(ApprovalKind::Tool, "agent_send", None),
            vec![ApprovalScope::Once, ApprovalScope::Session]
        );
        assert!(scopes_for(ApprovalKind::Tool, "agent", None).contains(&ApprovalScope::Always));
    }

    #[test]
    fn scopes_offer_session_dir_only_for_the_read_triad() {
        let read = scopes_for(ApprovalKind::Tool, "read", None);
        assert_eq!(read.last(), Some(&ApprovalScope::SessionDir));
        let bash = scopes_for(ApprovalKind::Tool, "bash", None);
        assert_eq!(
            bash,
            vec![
                ApprovalScope::Once,
                ApprovalScope::Session,
                ApprovalScope::Always
            ]
        );
    }

    #[test]
    fn plan_and_mode_offer_only_once() {
        for kind in [ApprovalKind::Plan, ApprovalKind::Mode] {
            assert_eq!(scopes_for(kind, "read", None), vec![ApprovalScope::Once]);
        }
    }

    #[test]
    fn floored_writes_and_inline_launches_offer_only_once() {
        let once = vec![ApprovalScope::Once];
        let floored = Some(CardReason::FlooredWrite);
        assert_eq!(scopes_for(ApprovalKind::Tool, "write", floored), once);
        assert_eq!(
            scopes_for(ApprovalKind::Rhai, "write (rhai)", floored),
            once
        );
        let inline = Some(CardReason::InlineLaunch);
        assert_eq!(scopes_for(ApprovalKind::Rhai, "rhai", inline), once);
    }

    #[test]
    fn grant_earning_reasons_keep_every_scope() {
        let full = scopes_for(ApprovalKind::Tool, "bash", None);
        for (kind, tool, card) in [
            (ApprovalKind::Tool, "bash", CardReason::RootEscape),
            (ApprovalKind::Rhai, "rhai", CardReason::ScriptLaunch),
            (ApprovalKind::Rhai, "rhai", CardReason::LaunchEscape),
        ] {
            assert_eq!(scopes_for(kind, tool, Some(card)), full, "{card:?}");
        }
        // An escape grant is exact-path: no directory scope on the triad.
        let read = scopes_for(ApprovalKind::Tool, "read", Some(CardReason::RootEscape));
        assert_eq!(read, full);
    }

    #[test]
    fn headless_starts_only_an_in_root_launch() {
        assert!(headless_starts(Some(CardReason::ScriptLaunch)));
        assert!(headless_starts(Some(CardReason::InlineLaunch)));
        for card in [
            None,
            Some(CardReason::LaunchEscape),
            Some(CardReason::RootEscape),
            Some(CardReason::FlooredWrite),
        ] {
            assert!(!headless_starts(card), "{card:?}");
        }
    }

    #[test]
    fn approval_stamps_scopes_and_time() {
        let s = SessionId::new("s");
        let a = approval(&s, "r1", ApprovalKind::Tool, "grep", "{}", "why", None);
        assert_eq!(a.scopes, scopes_for(ApprovalKind::Tool, "grep", None));
        assert!(a.since_ms > 0);
        assert_eq!((a.tool.as_str(), a.input.as_str()), ("grep", "{}"));
    }

    #[test]
    fn a_waiter_that_gave_up_is_not_listed() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        // A timed-out park drops its receiver but never removes its entry.
        drop(pending.register_approval(
            approval(&s, "r", ApprovalKind::Tool, "bash", "ls", "why", None),
            "bash",
            false,
        ));
        assert!(pending.approvals(|_| true).is_empty());
        assert!(pending.snapshot(None).is_empty());
    }

    #[test]
    fn list_pending_filters_to_the_root_subtree_and_skips_questions() {
        let pending = PendingDecisions::default();
        let agents = AgentRegistry::default();
        let (a, b) = (SessionId::new("a"), SessionId::new("b"));
        let _q = pending.register(&a, "q", "ask_user", "q");
        let _ra = pending.register_approval(
            approval(&a, "ra", ApprovalKind::Tool, "bash", "ls", "r", None),
            "bash",
            false,
        );
        let _rb = pending.register_approval(
            approval(
                &b,
                "rb",
                ApprovalKind::Mode,
                "request_mode",
                "{}",
                "r",
                None,
            ),
            "build",
            false,
        );
        let all = list_pending(&pending, &agents, None);
        assert_eq!(
            all.iter()
                .map(|p| p.request_id.as_str())
                .collect::<Vec<_>>(),
            ["ra", "rb"]
        );
        let only_a = list_pending(&pending, &agents, Some(&a));
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].request_id, "ra");
    }
}
