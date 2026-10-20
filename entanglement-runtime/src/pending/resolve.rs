//! Delivering a head's decision to its parked waiter (#156): only to the
//! kind of park it answers (ADR-0245), with an unoffered scope clamped
//! (ADR-0230).

use entanglement_core::{ApprovalOutcome, ApprovalScope, SessionId};

use super::{resolved, Parked, PendingDecisions};
use crate::seam::Decision;

/// What [`PendingDecisions::resolve`] did with a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a `Misaimed` decision should be refused to its sender"]
pub enum Resolution {
    /// The card was taken: its waiter got the decision, or had already
    /// given up (an approval card then reports `Expired`).
    Delivered,
    /// No card holds the id (already resolved, or never parked): a no-op.
    Absent,
    /// The card is the other kind — a question decision aimed at an
    /// approval card, or an `Approve`/`Reject` at an `ask_user` question.
    /// The card stays parked; `kind` is its [`Parked::kind`] (ADR-0245).
    Misaimed { kind: &'static str },
}

impl PendingDecisions {
    /// Deliver `decision` to the waiter for `(session, request_id)`, if one is
    /// still parked. A no-op when none is (already resolved, or a decision for an
    /// unknown/duplicate request). A dropped receiver (the waiter already unwound)
    /// is likewise harmless. A decision of the wrong kind for the card is
    /// refused and the card stays parked ([`Resolution::Misaimed`]): a
    /// question park only takes a question decision, an approval park only
    /// an approval one (ADR-0245). An `Approve` scope the card did not offer is
    /// clamped to `Once` ([`clamp_scope`]). A card moved to a compaction
    /// successor answers under its predecessor's id and its new home's
    /// (ADR-0238), and under the id of any home it was moved off (ADR-0242).
    pub fn resolve(&self, session: &SessionId, request_id: &str, decision: Decision) -> Resolution {
        let mut guard = self.lock();
        let (tx, parked) = match guard.remove(session, request_id, decision.is_question()) {
            Ok(Some(found)) => found,
            Ok(None) => return Resolution::Absent,
            Err(kind) => return Resolution::Misaimed { kind },
        };
        // A waiter that gave up got its timeout, not this decision.
        let outcome = if tx.is_closed() {
            ApprovalOutcome::Expired
        } else {
            resolved::outcome_of(&decision)
        };
        self.resolved(&parked, request_id, outcome);
        let _ = tx.send(clamp_scope(decision, &parked, request_id));
        Resolution::Delivered
    }
}

/// An `Approve` whose scope the parked card did not advertise becomes a
/// `Once` (ADR-0230). WHY here, not in each park: `scopes` is the contract
/// a head is shown, and a wire client can send any scope — on a floored
/// write outside the root an unclamped `Always` would persist a durable
/// extra-roots grant the card never offered. Parks with no wire approval
/// (`ask_user`) pass through untouched.
fn clamp_scope(decision: Decision, parked: &Parked, request_id: &str) -> Decision {
    let offered = |scope| {
        parked
            .approval
            .as_ref()
            .is_none_or(|a| a.scopes.contains(&scope))
    };
    match decision {
        Decision::Approve {
            scope,
            mode,
            run_all,
        } if !offered(scope) => {
            tracing::warn!(
                request_id,
                ?scope,
                "approval scope not offered by the card; clamped to once"
            );
            Decision::Approve {
                scope: ApprovalScope::Once,
                mode,
                run_all,
            }
        }
        other => other,
    }
}
