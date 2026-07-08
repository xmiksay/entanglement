//! Integration test for sub-agent spawn (#60). Drives the real runtime tool
//! executor: the parent model calls `spawn_agent`, the runtime starts a child
//! session, runs it to completion, and relays the child's final answer back to
//! the parent as a `ToolOutput` (ADR-0021/0010).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmSession,
    LlmStream, MessageRole, OutEvent, SessionId, ToolCall, ToolRegistry,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;

/// A content-routing LLM: it inspects the conversation rather than replaying a
/// fixed script, so the parent and the spawned child (which share the same
/// factory) behave differently and no infinite spawn recursion occurs.
struct RoutingLlm;

#[async_trait]
impl Llm for RoutingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        // Parent after the sub-agent's result folded back in → finish.
        if req.messages.iter().any(|m| m.role == MessageRole::Tool) {
            return Ok(stream_from_response(LlmResponse {
                text: "parent done".into(),
                tool_calls: vec![],
            }));
        }
        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.text.as_str())
            .unwrap_or("");
        let resp = if last_user == "child-task" {
            // The child answers directly (no further spawning).
            LlmResponse {
                text: "child-answer".into(),
                tool_calls: vec![],
            }
        } else {
            // The parent's first turn: delegate to a sub-agent.
            LlmResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "spawn1".into(),
                    name: "spawn_agent".into(),
                    input: r#"{"agent":"build","prompt":"child-task"}"#.into(),
                }],
            }
        };
        Ok(stream_from_response(resp))
    }
}

fn routing_config() -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(|| LlmSession::new(Box::new(RoutingLlm))),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn spawn_agent_relays_child_answer_to_parent() {
    let cfg = routing_config();
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    // Empty registry: `spawn_agent` is orchestration, handled before execution.
    spawn_tool_executor(&holly, ToolRegistry::new(), profiles);

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: parent.clone(),
            text: "parent-task".into(),
        })
        .await
        .unwrap();

    let mut child_started_under_parent = false;
    let mut saw_child_output = false;
    let mut parent_finished = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p),
                root: false,
                ..
            } if p == &parent => child_started_under_parent = true,
            OutEvent::ToolOutput {
                session, output, ..
            } if session == &parent && output == "child-answer" => saw_child_output = true,
            OutEvent::Done { session, .. } if session == &parent && saw_child_output => {
                parent_finished = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        child_started_under_parent,
        "a child session should start under the parent"
    );
    assert!(
        saw_child_output,
        "the child's answer should surface as the parent's spawn_agent ToolOutput"
    );
    assert!(
        parent_finished,
        "the parent should finish its turn after the sub-agent returns"
    );
}

/// An LLM that always tries to spawn another sub-agent on its first turn and
/// finishes once any tool result folds back in. Left unbounded it would spawn an
/// infinitely deep chain; the runtime's spawn guard (#76) must cap the depth.
struct RecursiveLlm;

#[async_trait]
impl Llm for RecursiveLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        if req.messages.iter().any(|m| m.role == MessageRole::Tool) {
            return Ok(stream_from_response(LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            }));
        }
        Ok(stream_from_response(LlmResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "spawn".into(),
                name: "spawn_agent".into(),
                input: r#"{"agent":"build","prompt":"recurse"}"#.into(),
            }],
        }))
    }
}

fn recursive_config() -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(|| LlmSession::new(Box::new(RecursiveLlm))),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn spawn_depth_is_bounded_and_refusal_is_relayed() {
    let cfg = recursive_config();
    let profiles = cfg.profiles.clone();
    let holly = Holly::spawn(cfg);
    spawn_tool_executor(&holly, ToolRegistry::new(), profiles);

    let root = SessionId::new("root");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: root.clone(),
            text: "start".into(),
        })
        .await
        .unwrap();

    let mut sessions_started = 0usize;
    let mut saw_depth_refusal = false;
    // The root finishes only after the whole capped chain unwinds back to it.
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted { .. } => sessions_started += 1,
            OutEvent::ToolOutput { output, .. } if output.contains("max spawn depth") => {
                saw_depth_refusal = true;
            }
            OutEvent::Done { session, .. } if session == &root => break,
            _ => {}
        }
    }

    assert!(
        saw_depth_refusal,
        "the deepest sub-agent's spawn should be refused with a max-depth message"
    );
    // root(0) + children at depth 1, 2, 3 = 4 sessions; the depth-3 spawn is refused.
    assert_eq!(
        sessions_started, 4,
        "the spawn tree should be capped at MAX_SPAWN_DEPTH below the root"
    );
}
