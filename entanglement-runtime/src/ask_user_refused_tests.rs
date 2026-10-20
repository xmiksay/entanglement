//! A refused `ask_user` park still resolves its call (ADR-0245): the turn
//! keeps running, so the model gets an error result, never silence.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, InMsg, Llm, LlmRequest, LlmResponse, LlmStream, ToolCall,
};

use super::*;
use crate::pending::test_support::detached_card;

const INPUT: &str = r#"{"questions":[{"question":"Which?","options":[{"label":"A"}]}]}"#;

/// Calls `ask_user` once as `q1`, then ends the turn.
struct AskOnce(Arc<Mutex<bool>>);
#[async_trait]
impl Llm for AskOnce {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let first = !std::mem::replace(&mut *self.0.lock().unwrap(), true);
        let tool_calls = if first {
            vec![ToolCall {
                id: "q1".into(),
                name: ASK_USER_TOOL.into(),
                input: INPUT.into(),
                provider_meta: None,
            }]
        } else {
            Vec::new()
        };
        Ok(stream_from_response(LlmResponse {
            text: if first { "" } else { "done" }.into(),
            tool_calls,
            ..Default::default()
        }))
    }
}

/// A background script's live card already holds `q1` on the session, so
/// the question's park is refused: no `UserQuestion` goes out, `ask_user`
/// moves no status (neither `WaitingAnswer` nor `Thinking`), and the call
/// replies with an error `ToolOutput`.
#[tokio::test]
async fn a_refused_question_park_replies_with_an_error() {
    let asked = Arc::new(Mutex::new(false));
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(AskOnce(asked.clone())) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let s = SessionId::new("s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(s.clone(), "go")).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolExec { request_id, .. })) if request_id == "q1" => break,
            Ok(Ok(_)) => {}
            other => panic!("no ToolExec: {other:?}"),
        }
    }
    let pending = PendingDecisions::default();
    let _card = pending.register_approval(detached_card(&s, "q1"), "edit");
    let open = OpenQuestions::default();
    tokio::time::timeout(
        Duration::from_secs(5),
        run_ask_user(
            holly.clone(),
            pending,
            open.clone(),
            s.clone(),
            "q1".into(),
            INPUT.into(),
            None,
        ),
    )
    .await
    .expect("a refused park returns, it does not hang");
    assert!(open.snapshot(None).is_empty(), "no open question is left");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolOutput {
                request_id,
                output,
                is_error,
                ..
            })) if request_id == "q1" => {
                assert!(is_error);
                assert!(output.contains("already parked"), "{output}");
                break;
            }
            // Nothing a head could answer is shown, and nothing waits.
            Ok(Ok(OutEvent::UserQuestion { request_id, .. })) if request_id == "q1" => {
                panic!("a refused park showed its question")
            }
            // `ask_user` moves no status (core's own `Working` is fine).
            Ok(Ok(OutEvent::Status {
                session,
                state: state @ (AgentState::WaitingAnswer | AgentState::Thinking),
            })) if session == s => panic!("a refused park moved the status: {state:?}"),
            Ok(Ok(_)) => {}
            other => panic!("no ToolOutput for q1: {other:?}"),
        }
    }
}

fn one_question() -> Vec<Question> {
    parse_input(INPUT)
}

/// A waiter decided before anything was shown — a `Stop` racing in right
/// after `register` — is not consumed: nothing is listed or emitted, and
/// the await still takes the decision instead of panicking on a polled
/// oneshot.
#[tokio::test]
async fn an_already_decided_waiter_shows_nothing_and_stays_awaitable() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let open = OpenQuestions::default();
    let s = SessionId::new("s");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = tx.send(seam::Decision::Stop);
    assert!(!show(&holly, &open, &s, "q1", &one_question(), &rx));
    assert!(open.snapshot(None).is_empty());
    assert!(sub.try_recv().is_err(), "nothing emitted");
    assert!(matches!(
        crate::pending::await_decision(rx).await,
        seam::Decision::Stop
    ));
    // A dropped sender is no decision yet: the question shows, and the
    // await then unwinds it like a `Stop`.
    let (tx, rx) = tokio::sync::oneshot::channel::<seam::Decision>();
    drop(tx);
    assert!(show(&holly, &open, &s, "q1", &one_question(), &rx));
    assert_eq!(open.snapshot(None).len(), 1);
}

/// A replace whose re-park is refused (a background card took the id in
/// between) still clears the open question it listed, moves the status off
/// `WaitingAnswer`, and shows no second question. Deterministic on the
/// current-thread runtime: the test's two registry calls run with no
/// yield between them.
#[tokio::test]
async fn a_refused_re_park_after_a_replace_clears_the_open_question() {
    let holly = Holly::spawn(EngineConfig::default());
    let pending = PendingDecisions::default();
    let open = OpenQuestions::default();
    let s = SessionId::new("s");
    let mut sub = holly.subscribe();
    let task = tokio::spawn(run_ask_user(
        holly.clone(),
        pending.clone(),
        open.clone(),
        s.clone(),
        "q1".into(),
        INPUT.into(),
        None,
    ));
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::UserQuestion { .. })) => break,
            Ok(Ok(_)) => {}
            other => panic!("no question: {other:?}"),
        }
    }
    let _ = pending.resolve(
        &s,
        "q1",
        seam::Decision::Replace {
            questions: one_question(),
        },
    );
    let _card = pending.register_approval(detached_card(&s, "q1"), "edit");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the call returns")
        .expect("no panic");
    assert!(
        open.snapshot(None).is_empty(),
        "the open question is cleared"
    );
    let mut after = Vec::new();
    while let Ok(ev) = sub.try_recv() {
        match ev {
            OutEvent::UserQuestion { .. } => after.push("question"),
            // The statuses `ask_user` itself moves; core adds its own.
            OutEvent::Status {
                state: AgentState::WaitingAnswer,
                ..
            } => after.push("waiting"),
            OutEvent::Status {
                state: AgentState::Thinking,
                ..
            } => after.push("thinking"),
            _ => {}
        }
    }
    assert_eq!(after, ["waiting", "thinking"]);
}
