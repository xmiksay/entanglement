//! Resume and detached approval cards (ADR-0237): a log's detached card is
//! stale unless this process still holds the session's live view.

use super::*;

fn card(session: &SessionId, seq: u64, request_id: &str, detached: bool) -> LogRecord {
    LogRecord::new(
        session.clone(),
        LogPayload::Out(OutEvent::ToolRequest {
            session: session.clone(),
            seq,
            request_id: request_id.into(),
            tool: "edit (rhai)".into(),
            input: "{}".into(),
            reason: None,
            scopes: Vec::new(),
            detached,
        }),
    )
}

fn front(reg: &SessionRegistry) -> Option<(String, bool)> {
    reg.active_view()
        .pending_tool_request()
        .map(|r| (r.request_id.clone(), r.detached))
}

/// A session this process never saw: its scripts died with the process
/// that ran them, so the log's detached card is dropped.
#[test]
fn a_cold_restore_drops_the_logs_detached_card() {
    let s = SessionId::new("past");
    let mut reg = SessionRegistry::new(SessionId::new("other"));
    reg.restore_from_records(s.clone(), &[card(&s, 1, "bg", true)]);
    assert_eq!(reg.active_id(), &s);
    assert_eq!(front(&reg), None);
}

/// A session hibernated by this process: its background script still
/// waits on the card (`stop_attached` spared it), so the live view's
/// detached card survives the rebuild.
#[test]
fn a_restore_over_a_live_view_keeps_its_detached_card() {
    let s = SessionId::new("hibernated");
    let mut reg = SessionRegistry::new(s.clone());
    reg.handle_out_event(live_card(&s));
    assert_eq!(front(&reg), Some(("bg".into(), true)));

    reg.restore_from_records(s.clone(), &[card(&s, 1, "bg", true)]);
    assert_eq!(front(&reg), Some(("bg".into(), true)));
    assert_eq!(reg.active_view().queued_approvals(), 0, "not duplicated");
}

fn live_card(s: &SessionId) -> OutEvent {
    match card(s, 1, "bg", true).payload {
        LogPayload::Out(ev) => ev,
        _ => unreachable!("card() builds an Out record"),
    }
}

/// ADR-0240: the log records a card's end, so a resumed view does not
/// show a card already resolved — a turn card included, which a restore
/// otherwise keeps.
#[test]
fn a_restore_drops_a_card_the_log_resolved() {
    let s = SessionId::new("past");
    let mut reg = SessionRegistry::new(SessionId::new("other"));
    let resolved = LogRecord::new(
        s.clone(),
        LogPayload::Out(OutEvent::ApprovalResolved {
            session: s.clone(),
            seq: 3,
            request_id: "turn".into(),
            outcome: entanglement_core::ApprovalOutcome::Approved,
        }),
    );
    let records = [
        card(&s, 1, "turn", false),
        card(&s, 2, "open", false),
        resolved,
    ];
    reg.restore_from_records(s.clone(), &records);
    assert_eq!(front(&reg), Some(("open".into(), false)));
    assert_eq!(reg.active_view().queued_approvals(), 0);
}
