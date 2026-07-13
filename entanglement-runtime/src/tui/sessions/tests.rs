use super::*;
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};

fn event(session: &SessionId, seq: u64, text: &str) -> OutEvent {
    OutEvent::TextDelta {
        session: session.clone(),
        seq,
        text: text.to_string(),
    }
}

#[test]
fn routes_events_to_the_right_session_without_cross_pollution() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(event(&a, 1, "hello-a"));
    reg.handle_out_event(event(&b, 1, "hello-b"));

    assert_eq!(reg.active_view().transcript().len(), 1);
    assert!(matches!(
        &reg.active_view().transcript()[0],
        TranscriptEntry::TextDelta { text } if text == "hello-a"
    ));

    let all = reg.all();
    assert_eq!(all.len(), 2);
    let b_view = all.iter().find(|(id, _)| **id == b).unwrap().1;
    assert_eq!(b_view.transcript().len(), 1);
}

#[test]
fn per_session_seq_dedupe_is_independent() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(event(&a, 1, "a1"));
    reg.handle_out_event(event(&b, 1, "b1"));
    reg.switch_to(b);
    assert_eq!(reg.active_view().transcript().len(), 1);
}

#[test]
fn background_approval_is_isolated_and_visible_in_sessions_list() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(OutEvent::ToolRequest {
        session: b.clone(),
        seq: 1,
        request_id: "t1".to_string(),
        tool: "read".to_string(),
        input: "{}".to_string(),
    });

    assert!(matches!(
        reg.active_view().approval_mode(),
        ApprovalMode::Normal
    ));

    let all = reg.all();
    let b_view = all.iter().find(|(id, _)| **id == b).unwrap().1;
    assert!(b_view.is_waiting_approval());

    reg.switch_to(b);
    assert!(matches!(
        reg.active_view().approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "t1"
    ));
}

#[test]
fn propose_plan_request_renders_accept_prompt_and_handoff_switches_session() {
    let plan_session = SessionId::new("plan-s");
    let mut reg = SessionRegistry::new(plan_session.clone());

    // A `propose_plan` ToolRequest surfaces the standard approval prompt and
    // exposes the plan input so the head can hand it off on approve (#141).
    reg.handle_out_event(OutEvent::ToolRequest {
        session: plan_session.clone(),
        seq: 1,
        request_id: "pp1".to_string(),
        tool: crate::propose_plan::PROPOSE_PLAN_TOOL.to_string(),
        input: r##"{"plan":"# Do it"}"##.to_string(),
    });
    assert!(matches!(
        reg.active_view().approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "pp1"
    ));
    let (_, tool, input) = reg
        .active_view()
        .pending_tool_request()
        .expect("pending propose_plan request");
    assert_eq!(tool, crate::propose_plan::PROPOSE_PLAN_TOOL);
    assert_eq!(crate::propose_plan::parse_plan(input), "# Do it");

    // The handoff mints a fresh root build session and switches to it.
    let build_session = SessionId::new("build-fresh");
    reg.adopt(build_session.clone());
    assert_eq!(reg.active_id(), &build_session);
    // The plan session stays alive after accept (a later re-propose mints
    // another fresh build session).
    assert!(reg.all().iter().any(|(id, _)| **id == plan_session));
}

#[test]
fn switch_round_trip_preserves_scroll_and_agent() {
    let a = SessionId::new("a");
    let mut reg = SessionRegistry::new(a.clone());
    let b = reg.create();

    reg.switch_to(a.clone());
    // Scroll is now clamped against draw-time metrics, so give session `a`
    // headroom (20 lines of content in a 10-row viewport) before freezing
    // it at a manual offset by scrolling up from the bottom.
    {
        let view = reg.active_view_mut();
        view.set_viewport_metrics(20, 10);
        view.scroll_up(3);
    }
    assert_eq!(reg.active_view().scroll_offset(), 7);
    assert!(!reg.active_view().auto_follow());

    reg.switch_to(b.clone());
    assert_eq!(reg.active_view().scroll_offset(), 0);
    assert!(reg.active_view().auto_follow());

    reg.switch_to(a);
    assert_eq!(reg.active_view().scroll_offset(), 7);
    assert!(!reg.active_view().auto_follow());
}

#[test]
fn create_generates_unique_incrementing_ids() {
    let mut reg = SessionRegistry::new(SessionId::new("tui"));
    let s2 = reg.create();
    let s3 = reg.create();
    assert_eq!(s2, SessionId::new("tui-2"));
    assert_eq!(s3, SessionId::new("tui-3"));
    assert_eq!(reg.active_id(), &s3);
}

#[test]
fn create_skips_collisions_with_existing_sessions() {
    let a = SessionId::new("tui");
    let mut reg = SessionRegistry::new(a.clone());
    reg.handle_out_event(event(&SessionId::new("tui-2"), 1, "x"));

    let created = reg.create();
    assert_eq!(created, SessionId::new("tui-3"));
}

#[test]
fn acceptance_multiple_sessions_visible_in_modal_switching_renders_right_transcript() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let c = SessionId::new("c");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(event(&a, 1, "hello-a"));
    reg.handle_out_event(event(&b, 1, "hello-b"));
    reg.handle_out_event(event(&c, 1, "hello-c"));

    let all = reg.all();
    assert_eq!(all.len(), 3, "All sessions should be visible");

    assert_eq!(
        reg.active_view().transcript().len(),
        1,
        "Active session has 1 entry"
    );
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-a"
        ),
        "Active session 'a' shows correct transcript"
    );

    reg.switch_to(b.clone());
    assert_eq!(
        reg.active_view().transcript().len(),
        1,
        "After switch, active session has 1 entry"
    );
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-b"
        ),
        "After switch, session 'b' shows correct transcript"
    );

    reg.switch_to(c.clone());
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-c"
        ),
        "After switch to 'c', shows correct transcript"
    );

    reg.switch_to(a.clone());
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-a"
        ),
        "Switching back to 'a' still shows correct transcript"
    );
}

#[test]
fn acceptance_new_session_created_on_first_prompt_and_appears_in_list() {
    let initial = SessionId::new("initial");
    let mut reg = SessionRegistry::new(initial.clone());

    reg.handle_out_event(event(&initial, 1, "first message"));

    let new_session = reg.create();
    assert!(new_session.to_string().starts_with("initial-"));

    let all = reg.all();
    assert_eq!(all.len(), 2, "New session appears in list");

    assert!(
        all.iter().any(|(id, _)| *id == &new_session),
        "New session ID is in the list"
    );

    reg.switch_to(new_session.clone());
    reg.handle_out_event(event(&new_session, 1, "new session message"));

    let all = reg.all();
    assert!(
        all.iter()
            .find(|(id, _)| **id == new_session)
            .map(|(_, view)| !view.transcript().is_empty())
            .unwrap_or(false),
        "New session transcript exists"
    );
}

#[test]
fn restore_from_records_rebuilds_transcript_and_switches() {
    use crate::session_store::{LogPayload, LogRecord};

    let initial = SessionId::new("live");
    let restored = SessionId::new("old");
    let mut reg = SessionRegistry::new(initial.clone());

    let prompt = LogRecord::new(
        restored.clone(),
        LogPayload::In(InMsg::Prompt {
            session: restored.clone(),
            text: "My name is Miksa".to_string(),
        }),
    );
    let reply = LogRecord::new(
        restored.clone(),
        LogPayload::Out(OutEvent::TextDelta {
            session: restored.clone(),
            seq: 1,
            text: "Hello Miksa".to_string(),
        }),
    );
    // Approve is a non-Prompt inbound record — it must not enter the transcript.
    let approve = LogRecord::new(
        restored.clone(),
        LogPayload::In(InMsg::Approve {
            session: restored.clone(),
            request_id: "r1".to_string(),
            scope: Default::default(),
        }),
    );

    reg.restore_from_records(restored.clone(), &[prompt, reply, approve]);

    assert_eq!(
        reg.active_id(),
        &restored,
        "restored session becomes active"
    );
    let transcript = reg.active_view().transcript();
    assert_eq!(transcript.len(), 2);
    assert!(matches!(
        &transcript[0],
        TranscriptEntry::User { text, pending } if text == "My name is Miksa" && !pending
    ));
    assert!(matches!(
        &transcript[1],
        TranscriptEntry::TextDelta { text } if text == "Hello Miksa"
    ));

    // The restored id appears exactly once in the tab order.
    assert_eq!(
        reg.all().iter().filter(|(id, _)| **id == restored).count(),
        1
    );
}

#[test]
fn acceptance_events_from_inactive_sessions_dont_pollute_active_view() {
    let active = SessionId::new("active");
    let background = SessionId::new("background");
    let mut reg = SessionRegistry::new(active.clone());

    reg.handle_out_event(event(&active, 1, "active-1"));

    reg.handle_out_event(event(&background, 1, "background-1"));
    reg.handle_out_event(event(&background, 2, "background-2"));

    assert_eq!(
        reg.active_view().transcript().len(),
        1,
        "Active session only has its own events"
    );
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "active-1"
        ),
        "Active session not polluted by background events"
    );

    reg.switch_to(background.clone());
    assert_eq!(
        reg.active_view().transcript().len(),
        2,
        "Background session has its own events"
    );

    reg.handle_out_event(event(&active, 2, "active-2"));

    assert_eq!(
        reg.active_view().transcript().len(),
        2,
        "Background session not polluted by active events"
    );

    reg.switch_to(active.clone());
    assert_eq!(
        reg.active_view().transcript().len(),
        2,
        "Active session now has both its events"
    );
}
