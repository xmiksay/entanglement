//! The TUI drops an `ask_user` question resolved elsewhere (#894): on its
//! call's `ToolOutput`, before the turn ends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, Question, QuestionOption, Questions, SessionId,
    ToolCall,
};
use entanglement_runtime::tool_names::ASK_USER_TOOL;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::ToolRegistry;
use tokio::sync::broadcast::Receiver;

use super::App;

const INPUT: &str = r#"{"questions":[{"question":"Which?","options":[{"label":"A"}]}]}"#;

/// Asks two questions in one batch, then ends the turn.
struct TwoQuestions(Arc<Mutex<bool>>);
#[async_trait]
impl Llm for TwoQuestions {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let first = !std::mem::replace(&mut *self.0.lock().unwrap(), true);
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: ASK_USER_TOOL.into(),
            input: INPUT.into(),
            provider_meta: None,
        };
        Ok(stream_from_response(LlmResponse {
            text: if first { "" } else { "done" }.into(),
            tool_calls: if first {
                vec![call("q1"), call("q2")]
            } else {
                Vec::new()
            },
            ..Default::default()
        }))
    }
}

fn engine() -> Holly {
    let asked = Arc::new(Mutex::new(false));
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(TwoQuestions(asked.clone())) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let _executor = ToolExecutor::builder(&holly, ToolRegistry::new().shared())
        .agents(entanglement_runtime::agents::built_in_registry().expect("built-in agents"))
        .base(PermissionProfile::new(Permission::Allow))
        .spawn();
    holly
}

/// Fold every event into `app` until one `stop` accepts (folded too).
/// The turn must not end first: that would clear the questions anyway.
async fn fold_until(app: &mut App, sub: &mut Receiver<OutEvent>, stop: impl Fn(&OutEvent) -> bool) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("the awaited event arrives")
            .expect("the engine is up");
        assert!(!matches!(ev, OutEvent::Done { .. }), "the turn ended first");
        let last = stop(&ev);
        app.handle_out_event(ev);
        if last {
            return;
        }
    }
}

fn asked(id: &'static str) -> impl Fn(&OutEvent) -> bool {
    move |ev| matches!(ev, OutEvent::UserQuestion { request_id, .. } if request_id == id)
}

fn ids(app: &App) -> Vec<String> {
    app.sessions
        .active_view()
        .pending_question()
        .map(|q| q.request_id.clone())
        .into_iter()
        .collect()
}

/// Two heads on one engine: one answers, and the other's question view
/// clears on the answer's `ToolOutput` while the turn is still parked on
/// the second question.
#[tokio::test]
async fn a_question_answered_by_another_head_clears_before_the_turn_ends() {
    let holly = engine();
    let s = SessionId::new("s");
    let (mut answering, mut other) = (holly.subscribe(), holly.subscribe());
    let (mut a, mut b) = (App::new_for_test(s.clone()), App::new_for_test(s.clone()));
    holly.send(InMsg::prompt(s.clone(), "go")).await.unwrap();
    fold_until(&mut a, &mut answering, asked("q2")).await;
    fold_until(&mut b, &mut other, asked("q2")).await;
    assert_eq!(ids(&b), ["q1"]);

    holly
        .send(InMsg::answer_question(
            s.clone(),
            "q1",
            vec![vec!["A".into()]],
        ))
        .await
        .unwrap();
    a.advance_question();
    fold_until(
        &mut b,
        &mut other,
        |ev| matches!(ev, OutEvent::ToolOutput { request_id, .. } if request_id == "q1"),
    )
    .await;
    assert_eq!(ids(&b), ["q2"], "q1 is gone, q2 takes the prompt");
    assert!(b.is_asking());
}

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
        tool: ASK_USER_TOOL.into(),
        output: "A".into(),
        content: Vec::new(),
        is_error: false,
        duration_ms: None,
        exit_code: None,
        envelope: None,
    }
}

fn typing(app: &mut App) {
    app.sessions.active_view_mut().question_begin_free_form();
    app.set_input_text("half typed".into());
}

/// A half-typed free-text answer must not pre-fill the question that takes
/// the prompt — whether the old one ended or was replaced.
#[test]
fn a_moved_question_prompt_takes_the_half_typed_answer() {
    for moved in [output(3, "q1"), ask(3, "q1", "Revised?")] {
        let mut app = App::new_for_test(SessionId::new("s"));
        app.handle_out_event(ask(1, "q1", "First?"));
        app.handle_out_event(ask(2, "q2", "Second?"));
        typing(&mut app);
        app.handle_out_event(moved);
        assert_eq!(app.input_text(), "");
    }
}

/// With no question left the text stays as an ordinary draft, and an event
/// that leaves the prompt alone keeps it too.
#[test]
fn a_half_typed_answer_stays_when_nothing_takes_the_prompt() {
    let mut app = App::new_for_test(SessionId::new("s"));
    app.handle_out_event(ask(1, "q1", "First?"));
    typing(&mut app);
    app.handle_out_event(ask(2, "q2", "Second?"));
    assert_eq!(app.input_text(), "half typed");
    app.handle_out_event(output(3, "q2"));
    app.handle_out_event(output(4, "q1"));
    assert_eq!(app.input_text(), "half typed");
    assert!(!app.is_asking());
}

/// Only a free-text answer in progress is taken: an ordinary draft typed
/// while a question is shown, but not answering it, stays.
#[test]
fn a_draft_not_typed_as_an_answer_stays_when_the_prompt_moves() {
    let mut app = App::new_for_test(SessionId::new("s"));
    app.handle_out_event(ask(1, "q1", "First?"));
    app.handle_out_event(ask(2, "q2", "Second?"));
    app.set_input_text("draft".into());
    app.handle_out_event(output(3, "q1"));
    assert_eq!(app.input_text(), "draft");
}
