//! Multi-user session identity (#522, ADR-0147): a root session spawned with
//! an explicit `user` carries it on `SessionStarted` and in `ListSessions`,
//! and a spawned child inherits its parent's user with no re-specification —
//! the "session-scoped identity" half of the multi-user embedder API. Drives
//! `Holly` end-to-end, mirroring `resume_children.rs`'s style.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, UserId,
};

struct Finisher;

#[async_trait]
impl Llm for Finisher {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

fn engine() -> Holly {
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(Finisher) as Box<dyn Llm>),
        ..EngineConfig::default()
    };
    Holly::spawn(cfg)
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

#[tokio::test]
async fn a_root_spawn_carries_its_explicit_user() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let root = SessionId::new("root");
    let alice = UserId::new("alice");

    holly
        .send(InMsg::Spawn {
            session: root.clone(),
            parent: None,
            predecessor: None,
            agent: "build".into(),
            prompt: "hi".into(),
            user: Some(alice.clone()),
            sponsored: false,
        })
        .await
        .unwrap();

    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == root),
    )
    .await;
    match ev {
        OutEvent::SessionStarted { user, .. } => assert_eq!(user, Some(alice)),
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

#[tokio::test]
async fn a_session_with_no_user_stays_single_user_mode() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let root = SessionId::new("root-anon");

    holly.send(InMsg::prompt(root.clone(), "hi")).await.unwrap();

    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == root),
    )
    .await;
    match ev {
        OutEvent::SessionStarted { user, .. } => {
            assert_eq!(user, None, "a bare Prompt never carries a user")
        }
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

#[tokio::test]
async fn a_spawned_child_inherits_its_parents_user_without_being_told() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    let alice = UserId::new("alice");

    holly
        .send(InMsg::Spawn {
            session: parent.clone(),
            parent: None,
            predecessor: None,
            agent: "build".into(),
            prompt: "hi".into(),
            user: Some(alice.clone()),
            sponsored: false,
        })
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == parent),
    )
    .await;

    // The child's own `Spawn` carries no `user` — core resolves it from the
    // live parent instead of requiring the caller to re-specify it.
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "build".into(),
            prompt: "subtask".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();

    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == child),
    )
    .await;
    match ev {
        OutEvent::SessionStarted {
            user,
            parent: child_parent,
            ..
        } => {
            assert_eq!(
                user,
                Some(alice),
                "the child must inherit its parent's user"
            );
            assert_eq!(child_parent, Some(parent.clone()));
        }
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

#[tokio::test]
async fn list_sessions_surfaces_each_sessions_user() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let alice_root = SessionId::new("alice-root");
    let bob_root = SessionId::new("bob-root");
    let alice = UserId::new("alice");
    let bob = UserId::new("bob");

    for (session, user) in [
        (alice_root.clone(), alice.clone()),
        (bob_root.clone(), bob.clone()),
    ] {
        holly
            .send(InMsg::Spawn {
                session: session.clone(),
                parent: None,
                predecessor: None,
                agent: "build".into(),
                prompt: "hi".into(),
                user: Some(user),
                sponsored: false,
            })
            .await
            .unwrap();
        recv_until(
            &mut sub,
            move |e| matches!(e, OutEvent::SessionStarted { session: s, .. } if *s == session),
        )
        .await;
    }

    let corr = "list-users".to_string();
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
    let alice_info = sessions
        .iter()
        .find(|i| i.session == alice_root)
        .expect("alice's session is listed");
    let bob_info = sessions
        .iter()
        .find(|i| i.session == bob_root)
        .expect("bob's session is listed");
    assert_eq!(alice_info.user, Some(alice));
    assert_eq!(bob_info.user, Some(bob));
}

#[tokio::test]
async fn an_empty_prompt_spawn_starts_idle_until_a_real_prompt_arrives() {
    // #674 (ADR-0174): a head authoring a trusted `Spawn` purely to bind a
    // session's `user` sends `prompt: ""` and relays the client's real
    // `Prompt` separately — the empty spawn prompt must not run a model turn
    // (which would stash the relayed prompt as mid-turn steering).
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingLlm(Arc<AtomicUsize>);

    #[async_trait]
    impl Llm for CountingLlm {
        async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(stream_from_response(LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            }))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(CountingLlm(calls2.clone())) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let mut sub = holly.subscribe();
    let root = SessionId::new("idle-root");

    holly
        .send(InMsg::Spawn {
            session: root.clone(),
            parent: None,
            predecessor: None,
            agent: "build".into(),
            prompt: String::new(),
            user: Some(UserId::new("alice")),
            sponsored: false,
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == root),
    )
    .await;
    let OutEvent::SessionStarted { user, .. } = ev else {
        unreachable!()
    };
    assert_eq!(user, Some(UserId::new("alice")));

    // Give a wrongly-queued empty turn time to run, then assert none did.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no turn on an empty spawn prompt"
    );

    // The relayed real prompt runs an ordinary turn.
    holly
        .send(InMsg::prompt(root.clone(), "hello"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == root),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
