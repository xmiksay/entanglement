//! Telling every head a card left the registry (#883, ADR-0240).
//!
//! WHY: a head drops its cards on a decision it sent itself, a `Stop` and
//! a terminal `Status`, but a card can also go where no head sees it — a
//! mode's `question_timeout` expiring a background script's card, another
//! head answering it. `OutEvent::ApprovalResolved` names the card on its
//! home session whatever removed it. Every call runs under the registry
//! lock, the same lock the park's `ToolRequest` and a move's announce are
//! emitted under (ADR-0238 §6), so a card's end always follows its card and
//! names its current home.

use entanglement_core::{ApprovalOutcome, OutEvent};

use super::{Parked, PendingDecisions};
use crate::seam::Decision;

impl PendingDecisions {
    /// Report `parked` gone. Only an approval card is reported: an
    /// `ask_user` question is turn-bound and has its own events
    /// (ADR-0146). Call with the registry lock held.
    pub(super) fn resolved(&self, parked: &Parked, request_id: &str, outcome: ApprovalOutcome) {
        let (Some(holly), Some(_)) = (&self.holly, &parked.approval) else {
            return;
        };
        holly.emit_for_session(&parked.home, |seq| OutEvent::ApprovalResolved {
            session: parked.home.clone(),
            seq,
            request_id: request_id.to_string(),
            outcome,
        });
    }

    /// A turn card a re-offer superseded (ADR-0238 §5) was unwound.
    pub(super) fn superseded(&self, card: Option<Parked>, request_id: &str) {
        if let Some(card) = card {
            self.resolved(&card, request_id, ApprovalOutcome::Stopped);
        }
    }
}

/// The outcome a decision delivered to a card reports. A frame aimed at an
/// approval's id that is no approval decision (`AnswerQuestion`,
/// `RetractQuestion`, `ReplaceQuestion`) unwinds the park like a `Stop`.
pub(super) fn outcome_of(decision: &Decision) -> ApprovalOutcome {
    match decision {
        Decision::Approve { .. } => ApprovalOutcome::Approved,
        Decision::Reject { .. } => ApprovalOutcome::Rejected,
        _ => ApprovalOutcome::Stopped,
    }
}

#[cfg(test)]
#[path = "resolved_tests.rs"]
mod tests;
