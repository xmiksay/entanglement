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
    /// Parked by a background script, not the turn (ADR-0237): it survives
    /// the turn's terminal `Status` ([`SessionView::drop_turn_approvals`]).
    pub detached: bool,
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

    /// Drops the whole approval queue — a turn interrupt (`Stop`, which
    /// unwinds a background script's card too) invalidates every parked
    /// request, not just the prompted one.
    pub fn clear_approval(&mut self) {
        self.approval_mode = ApprovalMode::Normal;
        self.pending_tool_requests.clear();
    }

    /// A terminal `Status` (#872): the finished turn's cards are void, but
    /// a background script's card is still parked in the runtime — and an
    /// attended park has no timeout — so dropping it would leave the script
    /// waiting on a card nobody can see.
    pub fn drop_turn_approvals(&mut self) {
        self.retain_approvals(|req| req.detached);
    }

    /// A view rebuilt from a session's log (resume). The log's detached
    /// cards are no evidence a script still waits: one this process never
    /// saw died with the process that ran it. `live` — this process's own
    /// view of the session, e.g. one it hibernated — is authoritative, so
    /// its detached cards are carried over instead (still parked: a
    /// hibernate spares them, `PendingDecisions::stop_attached`).
    pub fn adopt_detached_approvals(&mut self, live: Option<&SessionView>) {
        self.retain_approvals(|req| !req.detached);
        let carried = live
            .into_iter()
            .flat_map(|v| v.pending_tool_requests.iter())
            .filter(|req| req.detached)
            .cloned();
        self.pending_tool_requests.extend(carried);
        self.retain_approvals(|_| true);
    }

    /// Keep only the cards `keep` accepts. A prompt still on a kept card
    /// (even mid reject-reason) is left as is; otherwise the new front is
    /// prompted.
    fn retain_approvals(&mut self, keep: impl Fn(&PendingRequest) -> bool) {
        self.pending_tool_requests.retain(keep);
        let front = self.pending_tool_requests.front();
        let still_prompted = match &self.approval_mode {
            ApprovalMode::Normal => false,
            ApprovalMode::WaitingForApproval { request_id }
            | ApprovalMode::EnteringRejectReason { request_id } => {
                front.is_some_and(|req| &req.request_id == request_id)
            }
        };
        if !still_prompted {
            self.approval_mode = match front {
                Some(req) => ApprovalMode::WaitingForApproval {
                    request_id: req.request_id.clone(),
                },
                None => ApprovalMode::Normal,
            };
        }
    }

    pub fn is_waiting_approval(&self) -> bool {
        matches!(
            self.approval_mode,
            ApprovalMode::WaitingForApproval { .. } | ApprovalMode::EnteringRejectReason { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use entanglement_core::{AgentState, OutEvent, SessionId};

    use super::*;

    fn card(seq: u64, request_id: &str, detached: bool) -> OutEvent {
        OutEvent::ToolRequest {
            session: SessionId::new("s"),
            seq,
            request_id: request_id.into(),
            tool: "bash (rhai)".into(),
            input: "{}".into(),
            reason: None,
            scopes: Vec::new(),
            detached,
        }
    }

    fn idle() -> OutEvent {
        OutEvent::Status {
            session: SessionId::new("s"),
            state: AgentState::Idle,
        }
    }

    fn prompted(v: &SessionView) -> Option<&str> {
        match v.approval_mode() {
            ApprovalMode::WaitingForApproval { request_id } => Some(request_id),
            _ => None,
        }
    }

    #[test]
    fn terminal_status_keeps_a_detached_card_and_drops_the_turns() {
        let mut v = SessionView::new();
        v.apply_event(card(1, "turn", false));
        v.apply_event(card(2, "bg", true));
        assert_eq!(prompted(&v), Some("turn"));

        v.apply_event(idle());
        let front = v.pending_tool_request().expect("detached card kept");
        assert_eq!((front.request_id.as_str(), front.detached), ("bg", true));
        assert_eq!(v.queued_approvals(), 0);
        // The kept card is promoted to the prompt, so it can be answered.
        assert_eq!(prompted(&v), Some("bg"));
    }

    #[test]
    fn terminal_status_leaves_a_reject_reason_on_a_kept_card() {
        let mut v = SessionView::new();
        v.apply_event(card(1, "bg", true));
        v.set_approval_mode(ApprovalMode::EnteringRejectReason {
            request_id: "bg".into(),
        });
        v.apply_event(idle());
        assert!(matches!(
            v.approval_mode(),
            ApprovalMode::EnteringRejectReason { request_id } if request_id == "bg"
        ));
    }

    #[test]
    fn a_restored_view_with_no_live_view_drops_its_detached_cards_only() {
        let mut v = SessionView::new();
        v.apply_event(card(1, "bg", true));
        v.apply_event(card(2, "turn", false));
        v.adopt_detached_approvals(None);
        assert_eq!(prompted(&v), Some("turn"));
        assert_eq!(v.queued_approvals(), 0);
    }

    #[test]
    fn a_restored_view_carries_the_live_views_detached_cards() {
        let mut live = SessionView::new();
        live.apply_event(card(1, "bg-live", true));
        live.apply_event(card(2, "turn-live", false));
        let mut v = SessionView::new();
        v.apply_event(card(1, "bg-logged", true));
        v.adopt_detached_approvals(Some(&live));
        let front = v.pending_tool_request().expect("live card carried");
        assert_eq!(front.request_id, "bg-live");
        assert_eq!(v.queued_approvals(), 0, "only the live detached card");
        assert_eq!(prompted(&v), Some("bg-live"));
    }

    #[test]
    fn stop_clears_a_detached_card_too() {
        let mut v = SessionView::new();
        v.apply_event(card(1, "bg", true));
        v.clear_approval();
        assert!(v.pending_tool_request().is_none());
        assert!(!v.is_waiting_approval());
    }
}
