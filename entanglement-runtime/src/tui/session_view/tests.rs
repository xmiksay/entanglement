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
fn tool_call_deltas_grow_one_entry_then_the_assembled_call_finalizes_it() {
    // Streamed arg fragments (#194) coalesce into a single growing `ToolCall`
    // entry; the assembled `ToolCall` finalizes it with the authoritative input
    // instead of pushing a duplicate.
    let mut v = SessionView::new();
    assert!(v.apply_event(OutEvent::ToolCallDelta {
        session: sid(),
        seq: 1,
        request_id: "c1".into(),
        tool: "edit".into(),
        delta: r#"{"path":"#.into(),
    }));
    assert!(v.apply_event(OutEvent::ToolCallDelta {
        session: sid(),
        seq: 2,
        request_id: "c1".into(),
        tool: "edit".into(),
        delta: r#""a.rs"}"#.into(),
    }));
    // One entry so far, args growing live.
    assert_eq!(v.transcript().len(), 1);
    match &v.transcript()[0] {
        TranscriptEntry::ToolCall { tool, input, .. } => {
            assert_eq!(tool, "edit");
            assert_eq!(input, r#"{"path":"a.rs"}"#);
        }
        other => panic!("expected a ToolCall entry, got {other:?}"),
    }

    // The assembled call finalizes the same entry — no duplicate.
    assert!(v.apply_event(OutEvent::ToolCall {
        session: sid(),
        seq: 3,
        request_id: "c1".into(),
        tool: "edit".into(),
        input: r#"{"path":"a.rs"}"#.into(),
    }));
    assert_eq!(v.transcript().len(), 1);
    match &v.transcript()[0] {
        TranscriptEntry::ToolCall { input, .. } => assert_eq!(input, r#"{"path":"a.rs"}"#),
        other => panic!("expected a ToolCall entry, got {other:?}"),
    }
}

fn tool_call(seq: u64, request_id: &str, tool: &str, input: &str) -> OutEvent {
    OutEvent::ToolCall {
        session: sid(),
        seq,
        request_id: request_id.into(),
        tool: tool.into(),
        input: input.into(),
    }
}

fn tool_output(seq: u64, request_id: &str, tool: &str, output: &str) -> OutEvent {
    OutEvent::ToolOutput {
        session: sid(),
        seq,
        request_id: request_id.into(),
        tool: tool.into(),
        output: output.into(),
        content: vec![],
    }
}

#[test]
fn tool_output_folds_into_matching_call() {
    // The paired output folds into its `ToolCall.output` by `request_id` rather
    // than becoming a second transcript entry (#340).
    let mut v = SessionView::new();
    v.apply_event(tool_call(1, "c1", "read", r#"{"path":"a.rs"}"#));
    assert!(v.apply_event(tool_output(2, "c1", "read", "fn main() {}")));

    assert_eq!(v.transcript().len(), 1, "output must not push a new entry");
    match &v.transcript()[0] {
        TranscriptEntry::ToolCall { output, .. } => {
            assert_eq!(output.as_deref(), Some("fn main() {}"));
        }
        other => panic!("expected a ToolCall entry, got {other:?}"),
    }
}

#[test]
fn out_of_order_batch_outputs_fold_into_their_own_calls() {
    // Batch tool calls (#270) resolve in any order — each output must fold into
    // the call with its `request_id`, not the nearest unfilled one.
    let mut v = SessionView::new();
    v.apply_event(tool_call(1, "c1", "read", r#"{"path":"a.rs"}"#));
    v.apply_event(tool_call(2, "c2", "read", r#"{"path":"b.rs"}"#));
    // Second call resolves first.
    v.apply_event(tool_output(3, "c2", "read", "BODY_B"));
    v.apply_event(tool_output(4, "c1", "read", "BODY_A"));

    assert_eq!(v.transcript().len(), 2);
    let out = |i: usize| match &v.transcript()[i] {
        TranscriptEntry::ToolCall { output, .. } => output.clone(),
        other => panic!("expected a ToolCall entry, got {other:?}"),
    };
    assert_eq!(out(0).as_deref(), Some("BODY_A"));
    assert_eq!(out(1).as_deref(), Some("BODY_B"));
}

#[test]
fn unmatched_output_falls_back_to_standalone_entry() {
    // An output with no matching open call keeps the standalone notice.
    let mut v = SessionView::new();
    assert!(v.apply_event(tool_output(1, "orphan", "read", "stray")));
    assert_eq!(v.transcript().len(), 1);
    assert!(matches!(
        &v.transcript()[0],
        TranscriptEntry::ToolOutput { output, .. } if output == "stray"
    ));
}

#[test]
fn tool_call_without_deltas_still_pushes_an_entry() {
    // A provider that emits no streaming fragments lands on `ToolCall` directly.
    let mut v = SessionView::new();
    assert!(v.apply_event(OutEvent::ToolCall {
        session: sid(),
        seq: 1,
        request_id: "c1".into(),
        tool: "read".into(),
        input: "{}".into(),
    }));
    assert_eq!(v.transcript().len(), 1);
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

fn tool_request(seq: u64, request_id: &str, tool: &str) -> OutEvent {
    OutEvent::ToolRequest {
        session: sid(),
        seq,
        request_id: request_id.into(),
        tool: tool.into(),
        input: "{}".into(),
    }
}

#[test]
fn concurrent_tool_requests_queue_and_first_is_surfaced() {
    // Core batch-emits tool calls (#270), so a second ToolRequest can land
    // while the first is still prompted — it must queue, not overwrite (#273).
    let mut v = SessionView::new();
    v.apply_event(tool_request(1, "t1", "read"));
    v.apply_event(tool_request(2, "t2", "write"));

    assert!(matches!(
        v.approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "t1"
    ));
    assert_eq!(
        v.pending_tool_request().map(|(id, ..)| id.as_str()),
        Some("t1")
    );
    assert_eq!(v.queued_approvals(), 1);
}

#[test]
fn advancing_approval_promotes_next_queued_request() {
    // Approve and reject share this path: the answered front pops and the
    // next parked request is prompted immediately with its own request_id.
    let mut v = SessionView::new();
    v.apply_event(tool_request(1, "t1", "read"));
    v.apply_event(tool_request(2, "t2", "write"));

    v.advance_approval();
    assert!(matches!(
        v.approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "t2"
    ));
    assert_eq!(
        v.pending_tool_request().map(|(_, tool, _)| tool.as_str()),
        Some("write")
    );
    assert_eq!(v.queued_approvals(), 0);

    v.advance_approval();
    assert!(matches!(v.approval_mode(), ApprovalMode::Normal));
    assert!(v.pending_tool_request().is_none());
}

#[test]
fn advancing_from_reject_reason_entry_promotes_next_request() {
    // A reject typed via the reason box must also promote the next parked
    // request, not leave the mode stuck on the popped one.
    let mut v = SessionView::new();
    v.apply_event(tool_request(1, "t1", "read"));
    v.apply_event(tool_request(2, "t2", "write"));
    v.set_approval_mode(ApprovalMode::EnteringRejectReason {
        request_id: "t1".into(),
    });

    v.advance_approval();
    assert!(matches!(
        v.approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "t2"
    ));
}

#[test]
fn terminal_status_drops_the_whole_approval_queue() {
    let mut v = SessionView::new();
    v.apply_event(tool_request(1, "t1", "read"));
    v.apply_event(tool_request(2, "t2", "write"));

    v.apply_event(OutEvent::Status {
        session: sid(),
        state: AgentState::Idle,
    });
    assert!(!v.is_waiting_approval());
    assert!(v.pending_tool_request().is_none());
    assert_eq!(v.queued_approvals(), 0);
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

fn question(seq: u64, request_id: &str, text: &str) -> OutEvent {
    use entanglement_core::QuestionOption;
    OutEvent::UserQuestion {
        session: sid(),
        seq,
        request_id: request_id.into(),
        question: text.into(),
        options: vec![QuestionOption {
            label: "A".into(),
            description: None,
        }],
        allow_free_form: true,
    }
}

#[test]
fn concurrent_questions_queue_and_first_is_surfaced() {
    // Same batch rationale as approvals (#273): a second ask_user must queue
    // behind the prompted one instead of overwriting it.
    let mut v = SessionView::new();
    v.apply_event(question(1, "q1", "First?"));
    v.apply_event(question(2, "q2", "Second?"));

    assert!(v.is_asking());
    assert_eq!(
        v.pending_question().map(|q| q.request_id.as_str()),
        Some("q1")
    );
}

#[test]
fn advancing_question_promotes_next_with_fresh_selection() {
    let mut v = SessionView::new();
    v.apply_event(question(1, "q1", "First?"));
    v.apply_event(question(2, "q2", "Second?"));

    // Selection state on the front question must not bleed into the next.
    v.question_move(1);
    v.advance_question();

    let q = v.pending_question().expect("second question surfaced");
    assert_eq!(q.request_id, "q2");
    assert_eq!(q.selected, 0);
    assert!(!q.entering_free_form);

    v.advance_question();
    assert!(!v.is_asking());
}

#[test]
fn terminal_status_drops_the_whole_question_queue() {
    let mut v = SessionView::new();
    v.apply_event(question(1, "q1", "First?"));
    v.apply_event(question(2, "q2", "Second?"));

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

#[test]
fn supervisor_error_with_seq_zero_renders_even_after_seq_advances() {
    // ex-#159 / #157: a supervisor lifecycle error for an id with no live session
    // carries seq 0 (no counter to mint from). Once prior content has advanced
    // `last_seen_seq` past 0, a plain `seq > last_seen_seq` guard would drop it,
    // leaving the refusal structurally invisible. The seq-0 bypass renders it.
    let mut v = SessionView::new();
    assert!(v.apply_event(OutEvent::TextDelta {
        session: sid(),
        seq: 5,
        text: "working".into(),
    }));
    assert!(v.apply_event(OutEvent::Error {
        session: sid(),
        seq: 0,
        message: "session id is closed".into(),
    }));
    let errors = v
        .transcript()
        .iter()
        .filter(|e| matches!(e, TranscriptEntry::Error { .. }))
        .count();
    assert_eq!(errors, 1, "seq-0 supervisor error must render");
    // A genuinely-stale seq-bearing error still dedupes (bypass is seq-0 only).
    assert!(!v.apply_event(OutEvent::Error {
        session: sid(),
        seq: 3,
        message: "stale replay".into(),
    }));
}

#[test]
fn compacted_renders_a_fork_notice() {
    let mut v = SessionView::new();
    assert!(v.apply_event(OutEvent::Compacted {
        session: sid(),
        seq: 1,
        summary: "user asked for X, agent did Y".into(),
        kept: 0,
    }));
    let notice = v
        .transcript()
        .iter()
        .find_map(|e| match e {
            TranscriptEntry::ToolOutput {
                tool: Some(tool),
                output,
            } if tool == "compact" => Some(output.clone()),
            _ => None,
        })
        .expect("Compacted renders a tool-output-style notice");
    assert!(notice.contains("forked"));
    assert!(notice.contains("user asked for X, agent did Y"));
    // Replayed (seq not advancing) is deduped like any other content event.
    assert!(!v.apply_event(OutEvent::Compacted {
        session: sid(),
        seq: 1,
        summary: "replay".into(),
        kept: 0,
    }));
}
