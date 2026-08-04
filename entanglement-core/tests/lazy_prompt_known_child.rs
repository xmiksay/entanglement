//! The lazy-`Prompt` path (#639): an unknown-but-not-closed session id used to
//! be materialized as a blank session under the base `build` profile even when
//! it was already known — via `parent_links` — to be a sub-agent child. That
//! silently discarded the child's history and escalated it from its
//! restricted leaf profile to the most-privileged default, the exact
//! escalation the `Spawn` path's unknown-target case refuses.
//!
//! These drive `Holly` end-to-end, mirroring `hibernate.rs`/`resume_children.rs`'s
//! style.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, Message, OutEvent, Permission, PermissionProfile, ProfileRegistry,
    SessionId,
};

type Seen = Arc<Mutex<Vec<Vec<Message>>>>;
type Responses = Arc<Mutex<VecDeque<LlmResponse>>>;

struct RecordingLlm {
    responses: Responses,
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| LlmResponse {
                text: "assistant-reply".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// A leaf `Subagent` profile alongside the built-in `build`, mirroring the
/// `page-writer` example from the issue.
fn page_writer() -> AgentProfile {
    AgentProfile {
        name: "page-writer".into(),
        description: "leaf sub-agent".into(),
        mode: AgentMode::Subagent,
        system_prompt: "You write pages.".into(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Deny),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    }
}

fn engine() -> (Holly, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let responses: Responses = Arc::new(Mutex::new(VecDeque::new()));
    let seen2 = seen.clone();
    let mut profiles = ProfileRegistry::new();
    profiles.insert(page_writer());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                responses: responses.clone(),
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        profiles,
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), seen)
}

async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

/// Prompting a hibernated sub-agent child without an intervening `Resume`
/// must refuse — not silently blank-respawn it under `build` — so the caller
/// is told to resume instead of losing history and escalating the profile.
#[tokio::test]
async fn lazy_prompt_refuses_a_known_hibernated_child() {
    let (holly, seen) = engine();
    let mut sub = holly.subscribe();

    let root = SessionId::new("known-child-root");
    let child = SessionId::new("known-child-leaf");

    holly
        .send(InMsg::prompt(root.clone(), "start"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == root),
    )
    .await;

    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(root.clone()),
            predecessor: None,
            agent: "page-writer".into(),
            prompt: "write a page".into(),
            user: None,
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == child),
    )
    .await;

    holly.hibernate(child.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == child),
    )
    .await;

    let calls_before = seen.lock().unwrap().len();

    // Prompt the hibernated child directly, without resuming it first.
    holly
        .send(InMsg::prompt(child.clone(), "keep writing"))
        .await
        .unwrap();

    let ev = recv_until(&mut sub, |e| {
        e.session() == Some(&child) && !matches!(e, OutEvent::SessionHibernated { .. })
    })
    .await;
    match ev {
        OutEvent::Error { message, .. } => {
            assert!(
                message.to_lowercase().contains("resum"),
                "refusal must point the caller at `Resume`; got: {message}"
            );
        }
        other => panic!("expected a supervisor `Error` refusing the prompt, got {other:?}"),
    }

    // The child must not have been silently respawned and re-run.
    assert_eq!(
        seen.lock().unwrap().len(),
        calls_before,
        "a refused prompt must not reach the provider under a blank respawned session"
    );

    let corr = "q".to_string();
    holly
        .send(InMsg::ListSessions {
            correlation_id: corr.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { correlation_id, .. } if *correlation_id == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    assert!(
        !sessions.iter().any(|i| i.session == child),
        "a refused prompt must not register the child live under a different profile"
    );
}

/// A genuinely fresh (never-spawned) session id keeps the lazy-`Prompt`
/// path's single-user convenience: it still auto-creates a blank root under
/// `build`, unaffected by the known-child refusal.
#[tokio::test]
async fn lazy_prompt_still_auto_creates_a_fresh_root() {
    let (holly, _seen) = engine();
    let mut sub = holly.subscribe();

    let fresh = SessionId::new("brand-new-root");
    holly
        .send(InMsg::prompt(fresh.clone(), "hello"))
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == fresh),
    )
    .await;
    match ev {
        OutEvent::SessionStarted {
            profile,
            parent,
            root,
            ..
        } => {
            assert_eq!(profile, "build");
            assert_eq!(parent, None);
            assert!(root);
        }
        other => panic!("expected `SessionStarted`, got {other:?}"),
    }
}
