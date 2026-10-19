//! Parked-approval shapes: the payload of an [`OutEvent::PendingList`] reply
//! to [`InMsg::ListPending`] (#726), and the structured [`CardReason`] an
//! [`OutEvent::ToolRequest`] carries (ADR-0230), and the [`ApprovalOutcome`]
//! an [`OutEvent::ApprovalResolved`] reports (ADR-0240).

use serde::{Deserialize, Serialize};

use crate::protocol::{ApprovalScope, SessionId};
#[cfg(doc)]
use crate::protocol::{InMsg, OutEvent};

/// What a parked approval is for — which runtime path force-parked it, and so
/// what an [`InMsg::Approve`] on it means. `ask_user` questions are not
/// approvals (they take [`InMsg::AnswerQuestion`]) and are listed by
/// [`InMsg::ListQuestions`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// A host/MCP tool call the session's permission mode graded `Ask`, or an
    /// out-of-root access that forced a prompt (ADR-0109).
    Tool,
    /// `propose_plan` (always parks; `Approve.mode` picks the target mode).
    Plan,
    /// `request_mode` (always parks; approving switches the session's mode).
    Mode,
    /// A `rhai` script binding whose own call graded `Ask` (tool
    /// `<name> (rhai)`), or a `rhai` **launch card** (tool exactly `rhai`,
    /// ADR-0228) — the one answered with `Approve.run_all`.
    Rhai,
}

/// Why an approval card is not an ordinary graded `Ask` — the structured
/// fact a head decides on, so no head ever matches the card's text
/// (ADR-0230). Absent (`None`) on an ordinary card: a mode's `Ask` grade, a
/// credentialed `http` host approval, `propose_plan`, `request_mode`.
/// One value per card: where two facts meet, the one that limits what an
/// approval honors wins (a floored write outside the root is
/// [`FlooredWrite`](CardReason::FlooredWrite); the card text still carries
/// the escape warning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardReason {
    /// A host/MCP call reaching a path outside the project root (ADR-0109).
    RootEscape,
    /// A write of a trusted script or a skill's `SKILL.md`/`lib.rhai`
    /// (ADR-0228): it always asks, so an approval is once only.
    FlooredWrite,
    /// A `rhai` launch card for a `path` script inside the project root.
    ScriptLaunch,
    /// A `rhai` launch card for an inline script: no path to key a grant
    /// on, so an approval is once only.
    InlineLaunch,
    /// A `rhai` launch card for a `path` script outside the project root —
    /// a head with nobody to ask must refuse it.
    LaunchEscape,
}

/// Why a parked approval card left the runtime's registry — the payload of
/// [`OutEvent::ApprovalResolved`] (ADR-0240). Every exit maps to one value, so
/// a head drops the card by id whatever ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// An [`InMsg::Approve`], from any head.
    Approved,
    /// An [`InMsg::Reject`], from any head.
    Rejected,
    /// The mode's `question_timeout` elapsed with no decision.
    Expired,
    /// Unwound without a decision: a `Stop`, the session's end or hibernate
    /// (its turn's cards), a re-offer superseding a turn card, or any other
    /// inbound frame aimed at the card's id.
    Stopped,
}

/// One tool call parked awaiting [`InMsg::Approve`]/[`InMsg::Reject`], as
/// reported in an [`OutEvent::PendingList`] snapshot (#726). `tool`/`input`
/// are exactly what the still-open [`OutEvent::ToolRequest`] carried (so a
/// reconnecting head renders the same prompt it missed), plus the `session`
/// it belongs to — a `PendingList` can span many sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub session: SessionId,
    pub request_id: String,
    pub kind: ApprovalKind,
    pub tool: String,
    pub input: String,
    /// Human-readable why-it-parked, e.g. ``mode `build` grades `bash` Ask``.
    pub reason: String,
    /// The [`ApprovalScope`]s an `Approve` on this request actually honors —
    /// what a head should offer. Always starts with `Once`.
    pub scopes: Vec<ApprovalScope>,
    /// When the call parked, Unix epoch milliseconds.
    pub since_ms: u64,
    /// The structured card reason the `ToolRequest` carried (ADR-0230).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_reason: Option<CardReason>,
    /// The `detached` flag the `ToolRequest` carried (ADR-0237): a
    /// background script's card, which survives its turn's end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::OutEvent;

    fn request(reason: Option<CardReason>, scopes: Vec<ApprovalScope>) -> OutEvent {
        OutEvent::ToolRequest {
            session: SessionId::new("s1"),
            seq: 3,
            request_id: "r1".into(),
            tool: "rhai".into(),
            input: "1".into(),
            reason,
            scopes,
            detached: false,
        }
    }

    #[test]
    fn a_tool_request_round_trips_with_reason_and_scopes() {
        let ev = request(Some(CardReason::LaunchEscape), vec![ApprovalScope::Once]);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""reason":"launch_escape""#), "{json}");
        assert!(json.contains(r#""scopes":["once"]"#), "{json}");
        assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
    }

    /// A card with neither field serializes exactly as before ADR-0230, and
    /// an older frame without them still parses.
    #[test]
    fn a_tool_request_without_reason_or_scopes_keeps_the_old_shape() {
        let ev = request(None, Vec::new());
        let json = serde_json::to_string(&ev).unwrap();
        let old = r#"{"kind":"tool_request","session":"s1","seq":3,"request_id":"r1","tool":"rhai","input":"1"}"#;
        assert_eq!(json, old);
        assert_eq!(serde_json::from_str::<OutEvent>(old).unwrap(), ev);
    }

    /// ADR-0237: `detached` is omitted when false (every turn-owned card
    /// keeps its old shape, covered above) and round-trips when set.
    #[test]
    fn a_detached_tool_request_round_trips() {
        let mut ev = request(None, vec![ApprovalScope::Once]);
        if let OutEvent::ToolRequest { detached, .. } = &mut ev {
            *detached = true;
        }
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""detached":true"#), "{json}");
        assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
    }

    #[test]
    fn every_card_reason_has_a_snake_case_wire_name() {
        for (reason, wire) in [
            (CardReason::RootEscape, "root_escape"),
            (CardReason::FlooredWrite, "floored_write"),
            (CardReason::ScriptLaunch, "script_launch"),
            (CardReason::InlineLaunch, "inline_launch"),
            (CardReason::LaunchEscape, "launch_escape"),
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            assert_eq!(serde_json::from_str::<CardReason>(&json).unwrap(), reason);
        }
    }

    #[test]
    fn a_pending_approval_without_card_reason_parses() {
        let json = r#"{"session":"s","request_id":"r","kind":"tool","tool":"bash","input":"ls","reason":"why","scopes":["once"],"since_ms":1}"#;
        let a: PendingApproval = serde_json::from_str(json).unwrap();
        assert_eq!(a.card_reason, None);
        assert!(!a.detached);
        assert_eq!(serde_json::to_string(&a).unwrap(), json);
        let detached = PendingApproval {
            detached: true,
            ..a
        };
        let json = serde_json::to_string(&detached).unwrap();
        assert!(json.ends_with(r#""detached":true}"#), "{json}");
        assert_eq!(
            serde_json::from_str::<PendingApproval>(&json).unwrap(),
            detached
        );
    }

    /// ADR-0240: a stable snake_case wire shape for every outcome, routed
    /// and sequenced like any content event, and persisted (not live-only).
    #[test]
    fn approval_resolved_has_a_stable_wire_shape() {
        for (outcome, wire) in [
            (ApprovalOutcome::Approved, "approved"),
            (ApprovalOutcome::Rejected, "rejected"),
            (ApprovalOutcome::Expired, "expired"),
            (ApprovalOutcome::Stopped, "stopped"),
        ] {
            let ev = OutEvent::ApprovalResolved {
                session: SessionId::new("s1"),
                seq: 7,
                request_id: "r1".into(),
                outcome,
            };
            let json = serde_json::to_string(&ev).unwrap();
            let want = format!(
                r#"{{"kind":"approval_resolved","session":"s1","seq":7,"request_id":"r1","outcome":"{wire}"}}"#
            );
            assert_eq!(json, want);
            assert_eq!(serde_json::from_str::<OutEvent>(&json).unwrap(), ev);
            assert_eq!(ev.session(), Some(&SessionId::new("s1")));
            assert_eq!(ev.seq(), Some(7));
            assert!(!ev.is_live_only());
        }
    }

    /// Old-wire compatibility: a consumer built against an `OutEvent` that
    /// predates the variant rejects the frame outright (serde's internally
    /// tagged enums have no catch-all), so a wire consumer must decode the
    /// `kind` tag before the payload and skip kinds it does not know — and
    /// an unknown outcome is an error too, never a silent default.
    #[test]
    fn approval_resolved_is_skippable_by_kind_and_strict_on_outcome() {
        let frame = r#"{"kind":"approval_resolved","session":"s","seq":1,"request_id":"r","outcome":"approved"}"#;
        let value: serde_json::Value = serde_json::from_str(frame).unwrap();
        assert_eq!(value["kind"], "approval_resolved");
        assert_eq!(value["request_id"], "r");
        let future = frame.replace(r#""approved""#, r#""vanished""#);
        assert!(serde_json::from_str::<OutEvent>(&future).is_err());
    }
}
