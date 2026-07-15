//! Integration test for the runtime-owned `propose_plan` tool (#141, ADR-0042).
//!
//! The model calls `propose_plan`; the executor intercepts it on `ToolExec`
//! (before permission resolution, like `ask_user`) and **force-parks it on the
//! `Ask` path unconditionally** — a `ToolRequest` is emitted even under an
//! all-`Allow` profile. Approve folds an accepted message back (the engine holds
//! no plan state to record now, #231, ADR-0049); reject folds the typed reason
//! back. Neither emits `OutEvent::Plan`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::tool_names::PROPOSE_PLAN_TOOL;
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

/// A Holly whose scripted LLM calls `propose_plan` once, then ends the turn.
fn spawn_with_propose_plan_call(input: &str) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "p1".into(),
                name: PROPOSE_PLAN_TOOL.into(),
                input: input.into(),
                provider_meta: None,
            }],
        },
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
    // `propose_plan` is intercepted before the registry, so an empty registry is
    // fine. The default `ProfileRegistry` resolves the session to `build` (Allow-
    // all) — the request must still surface, proving the force-park.
    let _executor = spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    holly
}

/// The request must surface as a `ToolRequest` even though `build` is an
/// all-`Allow` profile — proving `propose_plan` force-parks regardless.
async fn await_request(holly: &Holly, sid: &SessionId) -> String {
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if let OutEvent::ToolRequest {
            request_id, tool, ..
        } = &ev
        {
            assert_eq!(tool, PROPOSE_PLAN_TOOL);
            return request_id.clone();
        }
    }
    panic!("expected a ToolRequest for propose_plan under an Allow profile");
}

#[tokio::test]
async fn approve_folds_accepted_output_and_records_no_plan() {
    let holly = spawn_with_propose_plan_call(r##"{"plan":"# Ship it\n1. do the thing"}"##);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    // Approve acks the model via a ToolOutput; the engine holds no plan state, so
    // no `OutEvent::Plan` is emitted (#231, ADR-0049). Collect until Done.
    let mut saw_plan = false;
    let mut got_output = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        match &ev {
            OutEvent::Plan { .. } => saw_plan = true,
            OutEvent::ToolOutput { tool, output, .. } if tool == PROPOSE_PLAN_TOOL => {
                assert_eq!(output, "plan accepted by the user");
                got_output = true;
            }
            OutEvent::Done { .. } => break,
            _ => {}
        }
    }
    assert!(got_output, "approve must fold an accepted ToolOutput back");
    assert!(
        !saw_plan,
        "approve must not emit a Plan snapshot (no engine state)"
    );
}

#[tokio::test]
async fn reject_folds_reason_and_records_no_plan() {
    let holly = spawn_with_propose_plan_call(r##"{"plan":"# Draft"}"##);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    let request_id = await_request(&holly, &sid).await;

    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("needs more detail on migrations".into()),
        })
        .await
        .unwrap();

    let mut saw_plan = false;
    let mut got_output = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        match &ev {
            OutEvent::Plan { .. } => saw_plan = true,
            OutEvent::ToolOutput { tool, output, .. } if tool == PROPOSE_PLAN_TOOL => {
                assert!(
                    output.contains("needs more detail on migrations"),
                    "reject must fold the typed reason back: {output}"
                );
                got_output = true;
            }
            OutEvent::Done { .. } => break,
            _ => {}
        }
    }
    assert!(got_output, "reject must fold a ToolOutput back");
    assert!(!saw_plan, "reject must not record a plan");
}
