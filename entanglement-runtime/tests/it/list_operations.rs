//! Integration tests for `InMsg::ListOperations` (#607, ADR-0161 §6) — the
//! head-facing counterpart to `poll`'s no-handle model surface. Exercises the
//! exact scenario ADR-0026 named and left open: a `background: true` sub-agent
//! launched but never polled must still be observable, attributed to the
//! session that launched it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    MessageRole, OperationKind, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;

fn finish(text: &str) -> LlmStream {
    stream_from_response(LlmResponse {
        text: text.into(),
        tool_calls: vec![],
    })
}

fn call(id: &str, name: &str, input: String) -> LlmStream {
    stream_from_response(LlmResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input,
            provider_meta: None,
        }],
    })
}

fn last_tool_seen(req: &LlmRequest<'_>) -> bool {
    req.messages.iter().any(|m| m.role == MessageRole::Tool)
}

fn last_user<'a>(req: &'a LlmRequest<'_>) -> &'a str {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .and_then(|m| m.content.iter().find_map(|p| p.as_text()))
        .unwrap_or("")
}

/// A parent that launches a sub-agent with `background: true` and then, on
/// seeing the launch confirmation, ends its turn **without ever polling the
/// handle** — the exact "launched-but-never-polled" scenario ADR-0026 named.
struct LaunchOnlyLlm;

#[async_trait]
impl Llm for LaunchOnlyLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        if last_user(&req) == "child-task" {
            return Ok(finish("child-answer"));
        }
        if last_tool_seen(&req) {
            return Ok(finish("parent done — never polled"));
        }
        Ok(call(
            "spawn1",
            "agent",
            r#"{"agent":"explore","prompt":"child-task","background":true}"#.into(),
        ))
    }
}

fn config() -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(|| Box::new(LaunchOnlyLlm) as Box<dyn Llm>),
        profiles: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn list_operations_returns_nothing_when_none_are_outstanding() {
    let holly = Holly::spawn(EngineConfig::default());
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        PermissionProfile::new(Permission::Allow),
    );
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::ListOperations {
            correlation_id: "c1".into(),
            session: None,
        })
        .await
        .unwrap();

    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::OperationList {
            correlation_id,
            operations,
        } = ev
        {
            assert_eq!(correlation_id, "c1");
            assert!(operations.is_empty());
            return;
        }
    }
    panic!("expected an OperationList reply");
}

/// The motivating ADR-0026/ADR-0161 §6 scenario: a launched-but-never-polled
/// sub-agent is still observable, attributed to the session that launched it.
#[tokio::test]
async fn list_operations_surfaces_a_launched_but_never_polled_agent() {
    let holly = Holly::spawn(config());
    spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        PermissionProfile::new(Permission::Allow),
    );
    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "parent-task"))
        .await
        .unwrap();

    // Wait for the parent's turn to end without ever calling `poll`.
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        if let OutEvent::Done { session, .. } = &ev {
            if session == &parent {
                break;
            }
        }
    }

    holly
        .send(InMsg::ListOperations {
            correlation_id: "c2".into(),
            session: Some(parent.clone()),
        })
        .await
        .unwrap();

    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::OperationList {
            correlation_id,
            operations,
        } = ev
        {
            assert_eq!(correlation_id, "c2");
            assert_eq!(operations.len(), 1, "the dangling child should be listed");
            assert_eq!(operations[0].session, parent);
            assert_eq!(operations[0].kind, OperationKind::Agent);
            assert_eq!(operations[0].launched_by, "explore");
            return;
        }
    }
    panic!("expected an OperationList reply");
}
