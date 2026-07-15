//! Integration test for the runtime-owned `ask_user` tool (#90, ADR-0027).
//!
//! The model calls `ask_user`; the executor intercepts it on `ToolExec` (before
//! permission resolution, like `agent_spawn`), emits `OutEvent::UserQuestion`,
//! and parks for the head's `InMsg::AnswerQuestion`. The picked answer is folded
//! back as the tool's `ToolResult` so the parent turn continues.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
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
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    holly
}

#[tokio::test]
async fn ask_user_emits_question_and_folds_answer_back() {
    let holly = spawn_with_ask_user_call(
        r#"{"question":"Which DB?","options":[{"label":"Postgres"},{"label":"SQLite"}],"allow_free_form":true}"#,
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
            question,
            options,
            allow_free_form,
            ..
        } = &ev
        {
            assert_eq!(question, "Which DB?");
            assert_eq!(options.len(), 2);
            assert_eq!(options[0].label, "Postgres");
            assert!(allow_free_form);
            request_id = Some(rid.clone());
            break;
        }
    }
    let request_id = request_id.expect("expected a UserQuestion event");

    // The user picks an option; the label flows back as the tool output.
    holly
        .send(InMsg::AnswerQuestion {
            session: sid.clone(),
            request_id,
            answer: "SQLite".into(),
        })
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
