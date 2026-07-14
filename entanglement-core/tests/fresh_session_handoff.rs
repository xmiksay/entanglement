//! Core pinning test for the plan-acceptance handoff (#141, ADR-0042).
//!
//! The handoff is head policy — the head mints a **fresh** session id and drives
//! it with `SetAgent { build }` + `Prompt { plan }`. This test pins the engine
//! contract that recipe relies on: a `SetAgent` on an unseen id lazily starts a
//! **root** session (no parent), switches it to `build` (`AgentChanged`), and the
//! following `Prompt` reaches the model as the session's first user message.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    MessageRole, OutEvent, SessionId,
};

/// An LLM that records the user messages of the first request it receives, so the
/// test can assert the plan arrived as the session's first user message.
struct CapturingLlm {
    seen_user_messages: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Llm for CapturingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let mut slot = self.seen_user_messages.lock().unwrap();
        if slot.is_empty() {
            *slot = req
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .map(|m| m.text().clone())
                .collect();
        }
        Ok(stream_from_response(LlmResponse {
            text: "building".into(),
            tool_calls: vec![],
        }))
    }
}

#[tokio::test]
async fn set_agent_then_prompt_on_fresh_id_starts_a_root_build_session() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_for_factory = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(CapturingLlm {
                seen_user_messages: seen_for_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut sub = holly.subscribe();

    // The head-side handoff recipe: a *fresh* id, never seen before.
    let fresh = SessionId::new_uuid();
    holly
        .send(InMsg::SetAgent {
            session: fresh.clone(),
            agent: "build".into(),
        })
        .await
        .unwrap();
    let plan = "# Approved plan\n1. implement it";
    holly
        .send(InMsg::prompt(fresh.clone(), plan))
        .await
        .unwrap();

    let mut started_as_root = None;
    let mut changed_to_build = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != &fresh {
            continue;
        }
        match &ev {
            OutEvent::SessionStarted { parent, .. } => started_as_root = Some(parent.is_none()),
            OutEvent::AgentChanged { agent, .. } if agent == "build" => changed_to_build = true,
            OutEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert_eq!(
        started_as_root,
        Some(true),
        "a fresh handoff session must start as a root (no parent)"
    );
    assert!(
        changed_to_build,
        "the session must switch to the build agent"
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|m| m == plan),
        "the accepted plan must reach the model as the first user message: {seen:?}"
    );
}
