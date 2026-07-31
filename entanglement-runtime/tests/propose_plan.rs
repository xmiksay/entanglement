//! Integration tests for the runtime-owned `propose_plan` tool (#141, ADR-0042,
//! amended by ADR-0138).
//!
//! The model calls `propose_plan`; the executor intercepts it on `ToolExec`
//! (before permission resolution, like `ask_user`) and **force-parks it on the
//! `Ask` path unconditionally** — a `ToolRequest` is emitted even under an
//! all-`Allow` profile. Per ADR-0138:
//!
//! - **Approve** spawns a sponsored `build` child of the plan session. The plan
//!   session parks on `WaitingAgent`; the build child runs under the `build`
//!   profile and its answer folds back as the `propose_plan` tool result, so
//!   the plan agent can cycle. The build child also receives an `OutEvent::Plan`
//!   snapshot of the accepted plan.
//! - **Reject** folds the typed reason back, unchanged.

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

/// A factory that hands the *first* session a `propose_plan`-then-ack script and
/// every subsequent session (the sponsored build child) a single fixed "build
/// finished" text response. This lets one approve flow drive both sessions
/// without the two scripts colliding on a shared stack.
fn plan_then_build_factory() -> Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync> {
    // First session (plan) calls propose_plan then acks the build result.
    let plan_responses = vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "p1".into(),
                name: PROPOSE_PLAN_TOOL.into(),
                input: r##"{"plan":"# Ship it"}"##.into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "plan agent acknowledged build result".into(),
            tool_calls: vec![],
        },
    ];
    // Subsequent sessions (the sponsored build child) emit a fixed answer.
    let build_responses = vec![LlmResponse {
        text: "build child finished the work".into(),
        tool_calls: vec![],
    }];
    let first = Arc::new(Mutex::new(true));
    let plan_responses = Arc::new(plan_responses);
    let build_responses = Arc::new(build_responses);
    Arc::new(move || {
        let is_first = {
            let mut g = first.lock().unwrap();
            let was = *g;
            *g = false;
            was
        };
        if is_first {
            Box::new(ScriptedLlm::new((*plan_responses).clone())) as Box<dyn Llm>
        } else {
            Box::new(ScriptedLlm::new((*build_responses).clone())) as Box<dyn Llm>
        }
    })
}

/// A Holly whose scripted LLM calls `propose_plan` once, then ends the turn.
fn spawn_with_propose_plan_call() -> Holly {
    let cfg = EngineConfig {
        llm_factory: plan_then_build_factory(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
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
async fn approve_spawns_sponsored_build_and_folds_answer_back() {
    let holly = spawn_with_propose_plan_call();
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

    // Approve spawns a sponsored build child. The plan session parks on
    // `WaitingAgent`; the build child runs, and its answer folds back as the
    // propose_plan tool result. Collect across both sessions until the plan
    // session's turn ends.
    let mut saw_waiting_agent = false;
    let mut saw_build_plan = false;
    let mut saw_build_text = false;
    let mut got_output = false;
    let mut build_session: Option<SessionId> = None;
    // The Plan snapshot for the build child can arrive ahead of its
    // SessionStarted (the runtime emits it right after Spawn, before the
    // session task starts), so match by content, not session id.
    let mut pending_plan_for_build: Option<String> = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::Status {
                session,
                state: entanglement_core::AgentState::WaitingAgent,
                ..
            } if session == &sid => {
                saw_waiting_agent = true;
            }
            // The build child is spawned via InMsg::Spawn → SessionStarted.
            OutEvent::SessionStarted {
                session,
                parent: Some(p),
                profile,
                ..
            } if p == &sid && profile == "build" => {
                build_session = Some(session.clone());
            }
            // The build child gets a Plan snapshot of the accepted plan (B6).
            OutEvent::Plan {
                session, content, ..
            } => {
                if build_session.as_ref() == Some(session) {
                    assert!(content.contains("Ship it"), "plan content: {content}");
                    saw_build_plan = true;
                } else if content.contains("Ship it") {
                    // Arrived before SessionStarted — stash and reconcile below.
                    pending_plan_for_build = Some(content.clone());
                }
            }
            OutEvent::TextDelta { session, text, .. } => {
                if build_session.as_ref() == Some(session) && text.contains("build child finished")
                {
                    saw_build_text = true;
                    // Reconcile a stashed plan now that we know the build id.
                    if let Some(c) = &pending_plan_for_build {
                        assert!(c.contains("Ship it"));
                        saw_build_plan = true;
                    }
                }
            }
            // The build's answer folds back as the propose_plan tool result.
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &sid && tool == PROPOSE_PLAN_TOOL => {
                assert!(
                    output.contains("build completed"),
                    "approve must fold the build result back: {output}"
                );
                assert!(
                    output.contains("build child finished the work"),
                    "the build child's answer must reach the plan agent: {output}"
                );
                got_output = true;
            }
            OutEvent::Done { session, .. } if session == &sid => break,
            _ => {}
        }
    }
    assert!(saw_waiting_agent, "plan session must park on WaitingAgent");
    assert!(
        build_session.is_some(),
        "a sponsored build child session must be spawned"
    );
    assert!(saw_build_plan, "build child must receive a Plan snapshot");
    assert!(saw_build_text, "build child's text must stream");
    assert!(
        got_output,
        "approve must fold the build's answer back as the propose_plan tool result"
    );
}

#[tokio::test]
async fn reject_folds_reason_and_records_no_plan() {
    // Reject doesn't spawn a build child, so a single-response plan LLM is
    // enough — the rejection folds back and the plan agent revises.
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "p1".into(),
                name: PROPOSE_PLAN_TOOL.into(),
                input: r##"{"plan":"# Draft"}"##.into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "revised".into(),
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
    let _executor = spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
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
