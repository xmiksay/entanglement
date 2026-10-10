//! The `agent` tool mints its child in the parent's lineage (#722, ADR-0211):
//! a sub-agent of a `{tenant}:` root keeps the tenant prefix, and the engine's
//! directory reports the child's parent and inherited user the moment it
//! starts — what an embedder's ownership check reads.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    MessageRole, OutEvent, Permission, PermissionProfile, SessionId, ToolCall, UserId,
};
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::ToolRegistry;

/// The parent calls a blocking `agent`; the child (prompted `child-task`)
/// answers; the parent finishes once the tool result is back.
struct SpawnOnce;

#[async_trait]
impl Llm for SpawnOnce {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let has_tool_result = req.messages.iter().any(|m| m.role == MessageRole::Tool);
        let is_child = req
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.text() == "child-task");
        let response = if is_child || has_tool_result {
            LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
                ..Default::default()
            }
        } else {
            LlmResponse {
                text: String::new(),
                tool_calls: vec![ToolCall::new(
                    "spawn1",
                    "agent",
                    r#"{"agent":"general","prompt":"child-task"}"#,
                )],
                ..Default::default()
            }
        };
        Ok(stream_from_response(response))
    }
}

#[tokio::test]
async fn an_agent_tool_child_keeps_the_tenant_prefix_and_owner() {
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(SpawnOnce) as Box<dyn Llm>),
        agents: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    };
    let profiles = cfg.agents.clone();
    let holly = Holly::spawn(cfg);
    ToolExecutor::builder(&holly, ToolRegistry::new().shared())
        .agents(profiles)
        .base(PermissionProfile::new(Permission::Allow))
        .spawn();

    let parent = SessionId::new("t1:root");
    let alice = UserId::new("alice");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: parent.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            prompt: "parent-task".into(),
            user: Some(alice.clone()),
            model: None,
        })
        .await
        .unwrap();

    let child = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted {
                    session,
                    parent: Some(p),
                    ..
                }) if p == parent => return session,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event stream closed before the child started"),
            }
        }
    })
    .await
    .expect("the agent tool spawned a child");

    assert!(child.0.starts_with("t1:s-"), "child id {child}");
    let info = holly
        .session_info(&child)
        .expect("a started child is in the directory");
    assert_eq!(info.parent, Some(parent));
    assert_eq!(info.user, Some(alice));
}
