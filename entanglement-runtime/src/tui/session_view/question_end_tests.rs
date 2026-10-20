//! An `ask_user` question leaves the view when its call's `ToolOutput`
//! arrives (#894) — answered by another head, retracted, or expired — and a
//! `UserQuestion` for a queued id replaces that entry (a `ReplaceQuestion`).

use entanglement_core::{OutEvent, Question, QuestionOption, Questions, SessionId};

use super::SessionView;
use crate::tui::format::attention_word;

fn ask(seq: u64, request_id: &str, text: &str) -> OutEvent {
    OutEvent::UserQuestion {
        session: SessionId::new("s"),
        seq,
        request_id: request_id.into(),
        questions: Questions(vec![Question {
            question: text.into(),
            options: vec![QuestionOption {
                label: "A".into(),
                description: None,
            }],
            multi_select: false,
        }]),
    }
}

fn output(seq: u64, request_id: &str) -> OutEvent {
    OutEvent::ToolOutput {
        session: SessionId::new("s"),
        seq,
        request_id: request_id.into(),
        tool: "ask_user".into(),
        output: "A".into(),
        content: Vec::new(),
        is_error: false,
        duration_ms: None,
        exit_code: None,
        envelope: None,
    }
}

fn front(v: &SessionView) -> Option<&str> {
    v.pending_question().map(|q| q.request_id.as_str())
}

fn texts(v: &SessionView) -> Vec<String> {
    v.pending_questions
        .iter()
        .map(|q| q.questions[0].question.clone())
        .collect()
}

/// The prompted call's result drops it mid-turn, promotes the next call,
/// and attention follows; the last one's result leaves nothing asking.
#[test]
fn a_calls_output_drops_its_question_and_promotes_the_next() {
    let mut v = SessionView::new();
    v.apply_event(ask(1, "q1", "First?"));
    v.apply_event(ask(2, "q2", "Second?"));
    v.apply_event(output(3, "q1"));
    assert_eq!(front(&v), Some("q2"));
    assert_eq!(texts(&v), ["Second?"]);
    assert!(attention_word(&v).is_some());
    v.apply_event(output(4, "q2"));
    assert!(!v.is_asking());
    assert!(attention_word(&v).is_none());
}

/// A queued call behind the prompt ends too, and the prompt stays put —
/// with the user's progress on it.
#[test]
fn a_queued_calls_output_leaves_the_prompted_one() {
    let mut v = SessionView::new();
    v.apply_event(ask(1, "q1", "First?"));
    v.apply_event(ask(2, "q2", "Second?"));
    v.question_begin_free_form();
    v.apply_event(output(3, "q2"));
    assert_eq!(front(&v), Some("q1"));
    assert!(v.pending_question().is_some_and(|q| q.entering_free_form));
}

/// Another call's output, another tool's output with the id, or a stale one
/// at or below the watermark, drops nothing.
#[test]
fn an_unrelated_or_stale_output_keeps_the_question() {
    let mut v = SessionView::new();
    v.apply_event(ask(2, "q1", "First?"));
    v.apply_event(output(3, "other"));
    let mut bash = output(4, "q1");
    if let OutEvent::ToolOutput { tool, .. } = &mut bash {
        *tool = "bash".into();
    }
    v.apply_event(bash);
    v.apply_event(output(1, "q1"));
    assert_eq!(front(&v), Some("q1"));
}

/// `ReplaceQuestion` re-emits under the same id: the entry is replaced in
/// place with fresh drafts, never queued twice, and a later output for
/// the id drops the replacement.
#[test]
fn a_replaced_question_takes_the_entrys_place() {
    let mut v = SessionView::new();
    v.apply_event(ask(1, "q1", "First?"));
    v.apply_event(ask(2, "q2", "Second?"));
    v.commit_question_answer(vec!["A".into()]);
    v.apply_event(ask(3, "q1", "Revised?"));
    assert_eq!(texts(&v), ["Revised?", "Second?"]);
    assert_eq!(front(&v), Some("q1"));
    assert!(v.pending_question().is_some_and(|q| !q.is_reviewing()));
    v.apply_event(output(4, "q1"));
    assert_eq!(texts(&v), ["Second?"]);
}

/// The same content again (a resume's re-offer) keeps the user's drafts.
#[test]
fn the_same_question_again_keeps_the_drafts() {
    let mut v = SessionView::new();
    v.apply_event(ask(1, "q1", "First?"));
    v.commit_question_answer(vec!["A".into()]);
    v.apply_event(ask(2, "q1", "First?"));
    assert_eq!(texts(&v), ["First?"]);
    assert!(v.pending_question().is_some_and(|q| q.is_reviewing()));
}
