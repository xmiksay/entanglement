use super::*;
use entanglement_core::SessionId;

fn sid() -> SessionId {
    SessionId::new("s1")
}

#[test]
fn seq_dedupe_drops_replay() {
    let mut v = SessionView::new();
    assert!(v.apply_event(OutEvent::TextDelta {
        session: sid(),
        seq: 1,
        text: "a".into(),
    }));
    assert!(!v.apply_event(OutEvent::TextDelta {
        session: sid(),
        seq: 1,
        text: "replay".into(),
    }));
    assert!(v.apply_event(OutEvent::TextDelta {
        session: sid(),
        seq: 2,
        text: "b".into(),
    }));
    assert_eq!(v.transcript().len(), 2);
}

#[test]
fn tool_request_sets_waiting_then_status_clears() {
    let mut v = SessionView::new();
    v.apply_event(OutEvent::ToolRequest {
        session: sid(),
        seq: 1,
        request_id: "t1".into(),
        tool: "read".into(),
        input: "{}".into(),
    });
    assert!(v.is_waiting_approval());
    assert_eq!(
        v.pending_tool_request().map(|(id, ..)| id.as_str()),
        Some("t1")
    );

    v.apply_event(OutEvent::Status {
        session: sid(),
        state: AgentState::Idle,
    });
    assert!(!v.is_waiting_approval());
    assert!(v.pending_tool_request().is_none());
}

#[test]
fn user_question_sets_pending_then_status_clears() {
    use entanglement_core::QuestionOption;
    let mut v = SessionView::new();
    v.apply_event(OutEvent::UserQuestion {
        session: sid(),
        seq: 1,
        request_id: "q1".into(),
        question: "Which?".into(),
        options: vec![
            QuestionOption {
                label: "A".into(),
                description: None,
            },
            QuestionOption {
                label: "B".into(),
                description: None,
            },
        ],
        allow_free_form: true,
    });
    assert!(v.is_asking());
    let q = v.pending_question().unwrap();
    // 2 options + the "Other" entry = 3 choices.
    assert_eq!(q.choice_count(), 3);
    assert_eq!(q.selected, 0);

    // Wrap past the last option onto "Other", then back to the top.
    v.question_move(-1);
    assert!(v.pending_question().unwrap().free_form_selected());
    v.question_move(1);
    assert_eq!(v.pending_question().unwrap().selected, 0);

    // A terminal status clears the pending question.
    v.apply_event(OutEvent::Status {
        session: sid(),
        state: AgentState::Done,
    });
    assert!(!v.is_asking());
}

#[test]
fn elapsed_tracks_running_then_freezes_on_end() {
    let mut v = SessionView::new();
    // Unknown until the session start is seen.
    assert_eq!(v.elapsed_secs(10_000), None);

    v.apply_event(OutEvent::SessionStarted {
        session: sid(),
        parent: Some(SessionId::new("root")),
        profile: "explore".into(),
        model: None,
        root: false,
        ts: 1_000,
    });
    // Running: measured against the current wall clock.
    assert_eq!(v.elapsed_secs(4_000), Some(3));
    assert!(!v.has_ended());

    v.apply_event(OutEvent::SessionEnded {
        session: sid(),
        ts: 6_500,
    });
    // Ended: fixed span regardless of the clock advancing further.
    assert!(v.has_ended());
    assert_eq!(v.elapsed_secs(999_999), Some(5));
}

#[test]
fn record_user_message_appears_before_streamed_reply() {
    // Regression for "user messages don't show in chat": recording a
    // prompt must insert a `User` entry into the transcript (and it must
    // not be subject to the seq dedupe guard, which only covers engine
    // `OutEvent`s).
    let mut v = SessionView::new();
    v.record_user_message("hello?".into());
    v.apply_event(OutEvent::TextDelta {
        session: sid(),
        seq: 1,
        text: "hi!".into(),
    });

    let entries = v.transcript();
    assert!(matches!(entries[0], TranscriptEntry::User { ref text, .. } if text == "hello?"));
    assert!(matches!(entries[1], TranscriptEntry::TextDelta { .. }));
    assert_eq!(entries.len(), 2);
}

fn user_pending(v: &SessionView) -> bool {
    matches!(
        v.transcript().first(),
        Some(TranscriptEntry::User { pending: true, .. })
    )
}

#[test]
fn reasoning_first_clears_pending_prompt() {
    // Regression (issue #103): a turn that opens with a thinking block must
    // still un-dim the user prompt, not only on the first text delta.
    let mut v = SessionView::new();
    v.record_user_message("go".into());
    assert!(user_pending(&v));

    v.apply_event(OutEvent::ReasoningDelta {
        session: sid(),
        seq: 1,
        text: "thinking...".into(),
    });
    assert!(!user_pending(&v));
}

#[test]
fn tool_call_first_clears_pending_prompt() {
    // Regression (issue #103): a turn that opens with a tool call must also
    // un-dim the user prompt.
    let mut v = SessionView::new();
    v.record_user_message("go".into());
    assert!(user_pending(&v));

    v.apply_event(OutEvent::ToolCall {
        session: sid(),
        seq: 1,
        request_id: "t1".into(),
        tool: "read".into(),
        input: "{}".into(),
    });
    assert!(!user_pending(&v));
}
