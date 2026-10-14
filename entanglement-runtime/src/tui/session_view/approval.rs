//! The approval queue: parked `ToolRequest` cards and the [`SessionView`]
//! methods that walk it (#273). Split out of `session_view.rs`.

use entanglement_core::ApprovalScope;

use super::{ApprovalMode, SessionView};

/// One parked `OutEvent::ToolRequest`, as the card renders it.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    pub request_id: String,
    pub tool: String,
    pub input: String,
    /// What an `Approve` on this card honors (ADR-0230) — the footer offers
    /// exactly these. Never empty: an older producer's card offers `Once`.
    pub scopes: Vec<ApprovalScope>,
}

impl PendingRequest {
    pub fn offers(&self, scope: ApprovalScope) -> bool {
        self.scopes.contains(&scope)
    }
}

impl SessionView {
    /// The front of the approval queue — the request currently prompted.
    pub fn pending_tool_request(&self) -> Option<&PendingRequest> {
        self.pending_tool_requests.front()
    }

    /// Approvals parked behind the prompted one (#273).
    pub fn queued_approvals(&self) -> usize {
        self.pending_tool_requests.len().saturating_sub(1)
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.approval_mode = mode;
    }

    /// Pops the front approval (just answered) and promotes the next queued
    /// request, if any, so its prompt surfaces immediately (#273).
    pub fn advance_approval(&mut self) {
        self.pending_tool_requests.pop_front();
        self.approval_mode = match self.pending_tool_requests.front() {
            Some(req) => ApprovalMode::WaitingForApproval {
                request_id: req.request_id.clone(),
            },
            None => ApprovalMode::Normal,
        };
    }

    /// Drops the whole approval queue — a turn interrupt or terminal status
    /// invalidates every parked request, not just the prompted one.
    pub fn clear_approval(&mut self) {
        self.approval_mode = ApprovalMode::Normal;
        self.pending_tool_requests.clear();
    }

    pub fn is_waiting_approval(&self) -> bool {
        matches!(
            self.approval_mode,
            ApprovalMode::WaitingForApproval { .. } | ApprovalMode::EnteringRejectReason { .. }
        )
    }
}
