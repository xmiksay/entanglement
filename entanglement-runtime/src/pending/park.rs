//! Parking a tool call for approval, and listing what is parked (#726).
//!
//! Every approval path (permission `Ask`, `propose_plan`, `request_mode`, a
//! `rhai` binding's `Ask`) parks through [`park_approval`], which registers
//! the waiter *and* emits the [`OutEvent::ToolRequest`] from one
//! [`PendingApproval`] value. WHY one value: `InMsg::ListPending` must report
//! exactly the prompt a reconnecting head missed — building the event and the
//! registry entry from the same struct means they can't drift.

use std::time::{SystemTime, UNIX_EPOCH};

use entanglement_core::{ApprovalKind, ApprovalScope, Holly, OutEvent, PendingApproval, SessionId};
use tokio::sync::oneshot;

use super::PendingDecisions;
use crate::agent_registry::AgentRegistry;
use crate::seam::Decision;

/// The scopes an `Approve` on a `kind`/`tool` park actually honors — what a
/// head should offer. `propose_plan`/`request_mode` ignore the scope (their
/// approver picks a *mode*, not a grant lifetime), so only `Once`;
/// `SessionDir` widens only the read-only triad (ADR-0126) — on any other
/// tool it would silently degrade to `Session`, so it isn't offered.
pub fn scopes_for(kind: ApprovalKind, tool: &str) -> Vec<ApprovalScope> {
    match kind {
        ApprovalKind::Plan | ApprovalKind::Mode => vec![ApprovalScope::Once],
        ApprovalKind::Tool | ApprovalKind::Rhai => {
            let mut scopes = vec![
                ApprovalScope::Once,
                ApprovalScope::Session,
                ApprovalScope::Always,
            ];
            if crate::tool_names::is_read_capability_member(tool) {
                scopes.push(ApprovalScope::SessionDir);
            }
            scopes
        }
    }
}

/// Build the [`PendingApproval`] for a call about to park, stamping its
/// scope options and park time.
pub fn approval(
    session: &SessionId,
    request_id: &str,
    kind: ApprovalKind,
    tool: impl Into<String>,
    input: impl Into<String>,
    reason: impl Into<String>,
) -> PendingApproval {
    let tool = tool.into();
    PendingApproval {
        session: session.clone(),
        request_id: request_id.to_string(),
        kind,
        scopes: scopes_for(kind, &tool),
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
/// `detail` is the short label a pending listing (`InMsg::ListPending`) shows; a
/// subtree-wide listing in a handle-less `poll` is #829. The prompt
/// mints a fresh per-session seq (#157).
pub fn park_approval(
    holly: &Holly,
    pending: &PendingDecisions,
    approval: PendingApproval,
    detail: impl Into<String>,
) -> oneshot::Receiver<Decision> {
    let session = approval.session.clone();
    let event = |seq| OutEvent::ToolRequest {
        session: approval.session.clone(),
        seq,
        request_id: approval.request_id.clone(),
        tool: approval.tool.clone(),
        input: approval.input.clone(),
    };
    let rx = pending.register_approval(approval.clone(), detail);
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

    #[test]
    fn scopes_offer_session_dir_only_for_the_read_triad() {
        let read = scopes_for(ApprovalKind::Tool, "read");
        assert_eq!(read.last(), Some(&ApprovalScope::SessionDir));
        let bash = scopes_for(ApprovalKind::Tool, "bash");
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
            assert_eq!(scopes_for(kind, "read"), vec![ApprovalScope::Once]);
        }
    }

    #[test]
    fn approval_stamps_scopes_and_time() {
        let s = SessionId::new("s");
        let a = approval(&s, "r1", ApprovalKind::Tool, "grep", "{}", "why");
        assert_eq!(a.scopes, scopes_for(ApprovalKind::Tool, "grep"));
        assert!(a.since_ms > 0);
        assert_eq!((a.tool.as_str(), a.input.as_str()), ("grep", "{}"));
    }

    #[test]
    fn a_waiter_that_gave_up_is_not_listed() {
        let pending = PendingDecisions::default();
        let s = SessionId::new("s");
        // A timed-out park drops its receiver but never removes its entry.
        drop(pending.register_approval(
            approval(&s, "r", ApprovalKind::Tool, "bash", "ls", "why"),
            "bash",
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
            approval(&a, "ra", ApprovalKind::Tool, "bash", "ls", "r"),
            "bash",
        );
        let _rb = pending.register_approval(
            approval(&b, "rb", ApprovalKind::Mode, "request_mode", "{}", "r"),
            "build",
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
