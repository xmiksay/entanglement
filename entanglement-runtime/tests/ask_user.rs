//! Integration test for the runtime-owned `ask_user` tool (#90, ADR-0027; #488
//! v2: several questions per call, an always-available free-text answer, and
//! per-question multi-select; #515: list/retract/replace an open question).
//!
//! The model calls `ask_user`; the executor intercepts it on `ToolExec` (before
//! permission resolution, like `agent_spawn`), emits `OutEvent::UserQuestion`,
//! and parks for the head's `InMsg::AnswerQuestion`. Every answer is folded
//! back as one `ToolResult` so the parent turn continues.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Question, QuestionOption, Questions, SessionId, ToolCall,
};
use entanglement_runtime::tool_names::ASK_USER_TOOL;
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;

/// Replays scripted responses in order, then plain text so the turn terminates.
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
}
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// A Holly whose scripted LLM calls `ask_user` once, then echoes the answer it
/// received back as its final text (so the test can assert the answer round-trip
/// reached the model).
fn spawn_with_ask_user_call(input: &str) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "q1".into(),
                name: ASK_USER_TOOL.into(),
                input: input.into(),
                provider_meta: None,
            }],
        },
        // The turn re-prompts after the tool result; the loop's default "done"
        // response ends it. The tool output is what we assert on.
        LlmResponse {
            text: "acknowledged".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    // `ask_user` is intercepted before the registry, so an empty registry is fine.
    let _executor = spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    holly
}

#[tokio::test]
async fn ask_user_emits_question_and_folds_answer_back() {
    let holly = spawn_with_ask_user_call(
        r#"{"questions":[{"question":"Which DB?","options":[{"label":"Postgres"},{"label":"SQLite"}]}]}"#,
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    // The executor surfaces the question with the parsed options.
    let mut request_id = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if let OutEvent::UserQuestion {
            request_id: rid,
            questions,
            ..
        } = &ev
        {
            assert_eq!(questions.0.len(), 1);
            assert_eq!(questions.0[0].question, "Which DB?");
            assert_eq!(questions.0[0].options.len(), 2);
            assert_eq!(questions.0[0].options[0].label, "Postgres");
            assert!(!questions.0[0].multi_select);
            request_id = Some(rid.clone());
            break;
        }
    }
    let request_id = request_id.expect("expected a UserQuestion event");

    // The user picks an option; the label flows back as the tool output.
    holly
        .send(InMsg::answer_question(
            sid.clone(),
            request_id,
            vec![vec!["SQLite".into()]],
        ))
        .await
        .unwrap();

    let mut got_answer = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        if let OutEvent::ToolOutput { tool, output, .. } = &ev {
            if tool == ASK_USER_TOOL {
                assert_eq!(output, "SQLite", "answer must fold back as the tool output");
                got_answer = true;
            }
        }
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    assert!(
        got_answer,
        "the ask_user tool output should carry the answer"
    );
}

#[tokio::test]
async fn ask_user_batches_multiple_questions_into_one_call() {
    let holly = spawn_with_ask_user_call(
        r#"{"questions":[
            {"question":"Which DB?","options":[{"label":"Postgres"},{"label":"SQLite"}]},
            {"question":"Which regions?","options":[{"label":"us-east"},{"label":"eu-west"}],"multi_select":true}
        ]}"#,
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut request_id = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if let OutEvent::UserQuestion {
            request_id: rid,
            questions,
            ..
        } = &ev
        {
            assert_eq!(questions.0.len(), 2);
            assert!(questions.0[1].multi_select);
            request_id = Some(rid.clone());
            break;
        }
    }
    let request_id = request_id.expect("expected a UserQuestion event");

    // One `AnswerQuestion` carries both answers — the second is a multi-select
    // pick of both regions.
    holly
        .send(InMsg::answer_question(
            sid.clone(),
            request_id,
            vec![
                vec!["SQLite".into()],
                vec!["us-east".into(), "eu-west".into()],
            ],
        ))
        .await
        .unwrap();

    let mut output_text = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        if let OutEvent::ToolOutput { tool, output, .. } = &ev {
            if tool == ASK_USER_TOOL {
                output_text = Some(output.clone());
            }
        }
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    let output_text = output_text.expect("the ask_user tool output should carry both answers");
    assert!(output_text.contains("SQLite"), "{output_text}");
    assert!(output_text.contains("us-east, eu-west"), "{output_text}");
}

#[tokio::test]
async fn ask_user_accepts_legacy_single_question_shape() {
    let holly = spawn_with_ask_user_call(
        r#"{"question":"Which DB?","options":[{"label":"Postgres"},{"label":"SQLite"}],"allow_free_form":true}"#,
    );
    let sid = SessionId::new("s1");
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut saw_question = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if let OutEvent::UserQuestion { questions, .. } = &ev {
            assert_eq!(questions.0.len(), 1);
            assert_eq!(questions.0[0].question, "Which DB?");
            saw_question = true;
            break;
        }
    }
    assert!(
        saw_question,
        "legacy single-question input must still parse"
    );
}

/// Wait for the next `UserQuestion` on `watch`, returning its `request_id`.
async fn await_user_question(watch: &mut tokio::sync::broadcast::Receiver<OutEvent>) -> String {
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if let OutEvent::UserQuestion { request_id, .. } = ev {
            return request_id;
        }
    }
    panic!("expected a UserQuestion event");
}

#[tokio::test]
async fn list_questions_returns_nothing_when_none_are_open() {
    // No scripted `ask_user` call at all — nothing should ever be open.
    let holly = Holly::spawn(EngineConfig::default());
    let _executor = spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::ListQuestions {
            correlation_id: "c1".into(),
            session: None,
        })
        .await
        .unwrap();

    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::QuestionList {
            correlation_id,
            questions,
        } = ev
        {
            assert_eq!(correlation_id, "c1");
            assert!(questions.is_empty());
            return;
        }
    }
    panic!("expected a QuestionList reply");
}

#[tokio::test]
async fn list_questions_returns_one_open_question() {
    let holly = spawn_with_ask_user_call(
        r#"{"questions":[{"question":"Which DB?","options":[{"label":"Postgres"},{"label":"SQLite"}]}]}"#,
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let request_id = await_user_question(&mut watch).await;

    holly
        .send(InMsg::ListQuestions {
            correlation_id: "c1".into(),
            session: None,
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::QuestionList { questions, .. } = ev {
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].session, sid);
            assert_eq!(questions[0].request_id, request_id);
            assert_eq!(questions[0].questions.0[0].question, "Which DB?");
            return;
        }
    }
    panic!("expected a QuestionList reply");
}

#[tokio::test]
async fn list_questions_spans_or_filters_multiple_sessions() {
    let holly = spawn_with_ask_user_call(
        r#"{"questions":[{"question":"Which DB?","options":[{"label":"Postgres"}]}]}"#,
    );
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let mut sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(a.clone(), "go")).await.unwrap();
    let req_a = await_user_question(&mut watch).await;
    holly.send(InMsg::prompt(b.clone(), "go")).await.unwrap();
    let req_b = await_user_question(&mut watch).await;

    // Global: both sessions' open questions.
    holly
        .send(InMsg::ListQuestions {
            correlation_id: "all".into(),
            session: None,
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::QuestionList {
            correlation_id,
            questions,
        } = ev
        {
            if correlation_id != "all" {
                continue;
            }
            assert_eq!(questions.len(), 2);
            let ids: Vec<_> = questions.iter().map(|q| q.request_id.clone()).collect();
            assert!(ids.contains(&req_a));
            assert!(ids.contains(&req_b));
            break;
        }
    }

    // Filtered: only session `a`'s.
    holly
        .send(InMsg::ListQuestions {
            correlation_id: "a-only".into(),
            session: Some(a.clone()),
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::QuestionList {
            correlation_id,
            questions,
        } = ev
        {
            if correlation_id != "a-only" {
                continue;
            }
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].session, a);
            assert_eq!(questions[0].request_id, req_a);
            return;
        }
    }
    panic!("expected the filtered QuestionList reply");
}

#[tokio::test]
async fn retract_question_resolves_the_waiter_without_stopping_the_turn() {
    let holly = spawn_with_ask_user_call(
        r#"{"questions":[{"question":"Which DB?","options":[{"label":"Postgres"}]}]}"#,
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let request_id = await_user_question(&mut watch).await;

    holly
        .send(InMsg::RetractQuestion {
            session: sid.clone(),
            request_id: request_id.clone(),
        })
        .await
        .unwrap();

    let mut got_withdrawal = false;
    let mut saw_done = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        if let OutEvent::ToolOutput { tool, output, .. } = &ev {
            if tool == ASK_USER_TOOL {
                assert!(output.contains("withdrew"), "{output}");
                got_withdrawal = true;
            }
        }
        // The rest of the turn is unaffected by a retract — it still reaches
        // `Done`, unlike a session-wide `Stop`.
        if matches!(ev, OutEvent::Done { .. }) {
            saw_done = true;
            break;
        }
    }
    assert!(got_withdrawal, "expected a withdrawal ToolOutput");
    assert!(saw_done, "the turn must continue to Done, not hang");

    // The registry is clear once retracted.
    holly
        .send(InMsg::ListQuestions {
            correlation_id: "after".into(),
            session: None,
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::QuestionList { questions, .. } = ev {
            assert!(questions.is_empty());
            return;
        }
    }
    panic!("expected a QuestionList reply");
}

#[tokio::test]
async fn replace_question_re_emits_a_revised_user_question() {
    let holly = spawn_with_ask_user_call(
        r#"{"questions":[{"question":"Which DB?","options":[{"label":"Postgres"}]}]}"#,
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let request_id = await_user_question(&mut watch).await;

    holly
        .send(InMsg::ReplaceQuestion {
            session: sid.clone(),
            request_id: request_id.clone(),
            questions: Questions(vec![Question {
                question: "Which DB, revised?".into(),
                options: vec![QuestionOption {
                    label: "MySQL".into(),
                    description: None,
                }],
                multi_select: false,
            }]),
        })
        .await
        .unwrap();

    // A second `UserQuestion` under the *same* request id, carrying the
    // revised content.
    let mut revised_seen = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if let OutEvent::UserQuestion {
            request_id: rid,
            questions,
            ..
        } = &ev
        {
            if rid == &request_id && questions.0[0].question == "Which DB, revised?" {
                assert_eq!(questions.0[0].options[0].label, "MySQL");
                revised_seen = true;
                break;
            }
        }
    }
    assert!(revised_seen, "expected a re-emitted, revised UserQuestion");

    // The call is still open under the same request id (not answered yet).
    holly
        .send(InMsg::ListQuestions {
            correlation_id: "mid".into(),
            session: None,
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::QuestionList { questions, .. } = ev {
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].request_id, request_id);
            assert_eq!(questions[0].questions.0[0].question, "Which DB, revised?");
            break;
        }
    }

    // Answering the revised call still folds back normally.
    holly
        .send(InMsg::answer_question(
            sid.clone(),
            request_id,
            vec![vec!["MySQL".into()]],
        ))
        .await
        .unwrap();
    let mut got_answer = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        if let OutEvent::ToolOutput { tool, output, .. } = &ev {
            if tool == ASK_USER_TOOL {
                assert_eq!(output, "MySQL");
                got_answer = true;
            }
        }
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    assert!(
        got_answer,
        "the revised call must still fold back an answer"
    );
}
