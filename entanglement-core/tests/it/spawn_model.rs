//! An explicit model on `InMsg::Spawn` (#725): resolved through the same
//! `model_resolver` a `SetModel` uses, bound before the first round, winning
//! over the agent's pin (ADR-0081's session memory > static pin), refusing the
//! spawn when it cannot resolve, and logged as an ordinary `ModelChanged` so
//! replay rebinds it exactly. Scaffolding is shared with `agent_model_pin`.

use std::sync::{Arc, Mutex};

use entanglement_core::{
    EngineConfig, Holly, InMsg, ModelChoice, ModelResolver, OutEvent, SessionId, UserId,
};

use super::agent_model_pin::{config, drain_until, is_done, profile, registry, resolver, Seen};

fn spawn(sid: &SessionId, agent: &str, model: Option<ModelChoice>) -> InMsg {
    InMsg::Spawn {
        session: sid.clone(),
        parent: None,
        predecessor: None,
        agent: agent.into(),
        prompt: "one".into(),
        user: None,
        model,
        mode: None,
    }
}

fn seen() -> Seen {
    Arc::new(Mutex::new(Vec::new()))
}

#[tokio::test]
async fn spawn_model_drives_the_first_round() {
    let (start, switched) = (seen(), seen());
    let holly = Holly::spawn(config(
        &start,
        &switched,
        registry([profile("general", None)]),
    ));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(spawn(
            &sid,
            "general",
            Some(ModelChoice::new("zai", "glm-b")),
        ))
        .await
        .unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;

    let started = evs
        .iter()
        .position(|e| matches!(e, OutEvent::SessionStarted { model, .. } if model.as_deref() == Some("glm-b")))
        .expect("SessionStarted announces the spawn model");
    let changed = evs
        .iter()
        .position(|e| matches!(e, OutEvent::ModelChanged { provider, model, .. } if provider == "zai" && model == "glm-b"))
        .expect("the spawn model is announced as ModelChanged");
    assert!(started < changed, "logged after SessionStarted, like a pin");
    assert!(start.lock().unwrap().is_empty(), "startup backend unused");
    assert_eq!(switched.lock().unwrap().clone(), vec![Some("glm-b".into())]);
}

#[tokio::test]
async fn spawn_model_wins_over_the_agent_pin() {
    let (start, switched) = (seen(), seen());
    let holly = Holly::spawn(config(
        &start,
        &switched,
        registry([
            profile("general", None),
            profile("plan", Some(("anthropic", "claude-x"))),
        ]),
    ));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(spawn(&sid, "plan", Some(ModelChoice::new("zai", "glm-c"))))
        .await
        .unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;

    let models: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            OutEvent::ModelChanged { model, .. } => Some(model.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(models, vec!["glm-c"], "the pin is never applied");
    assert_eq!(switched.lock().unwrap().clone(), vec![Some("glm-c".into())]);
}

/// An unresolvable choice refuses the spawn outright with a supervisor
/// `Error` (seq 0) — no session, no round on a model nobody asked for — and
/// leaves the id free, so a corrected retry spawns normally.
#[tokio::test]
async fn unresolvable_spawn_model_refuses_the_spawn() {
    let (start, switched) = (seen(), seen());
    let holly = Holly::spawn(config(
        &start,
        &switched,
        registry([profile("general", None)]),
    ));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(spawn(&sid, "general", Some(ModelChoice::new("nope", "x"))))
        .await
        .unwrap();
    let evs = drain_until(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
    match evs.last() {
        Some(OutEvent::Error {
            session,
            seq,
            message,
        }) => {
            assert_eq!(session, &sid);
            assert_eq!(*seq, 0, "a supervisor lifecycle error");
            assert!(message.contains("nope/x"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(
        !evs.iter()
            .any(|e| matches!(e, OutEvent::SessionStarted { .. })),
        "the session never started"
    );
    assert!(!holly.is_live(&sid));

    holly
        .send(spawn(
            &sid,
            "general",
            Some(ModelChoice::new("zai", "glm-b")),
        ))
        .await
        .unwrap();
    drain_until(&mut sub, is_done(&sid)).await;
    assert!(
        start.lock().unwrap().is_empty(),
        "no round ever ran on the default"
    );
    assert_eq!(switched.lock().unwrap().clone(), vec![Some("glm-b".into())]);
}

#[tokio::test]
async fn spawn_model_without_a_resolver_is_refused() {
    let start = seen();
    let mut cfg = config(&start, &seen(), registry([profile("general", None)]));
    cfg.model_resolver = None;
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(spawn(
            &sid,
            "general",
            Some(ModelChoice::new("zai", "glm-b")),
        ))
        .await
        .unwrap();
    let evs = drain_until(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
    assert!(matches!(
        evs.last(),
        Some(OutEvent::Error { message, .. }) if message.contains("not supported")
    ));
    assert!(!holly.is_live(&sid));
    assert!(start.lock().unwrap().is_empty());
}

/// The choice resolves under the session's effective user (#522), so a
/// multi-user embedder's per-user catalog/keys apply to it.
#[tokio::test]
async fn spawn_model_resolves_under_the_spawn_user() {
    let (start, switched) = (seen(), seen());
    let users: Arc<Mutex<Vec<Option<UserId>>>> = Arc::new(Mutex::new(Vec::new()));
    let inner = resolver(&switched);
    let recorded = users.clone();
    let recording: ModelResolver = Arc::new(move |user, provider, model| {
        recorded.lock().unwrap().push(user.cloned());
        inner(user, provider, model)
    });
    let holly = Holly::spawn(EngineConfig {
        model_resolver: Some(recording),
        ..config(&start, &switched, registry([profile("general", None)]))
    });
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    let mut msg = spawn(&sid, "general", Some(ModelChoice::new("zai", "glm-b")));
    if let InMsg::Spawn { user, .. } = &mut msg {
        *user = Some(UserId::new("alice"));
    }
    holly.send(msg).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;
    assert_eq!(
        users.lock().unwrap().first().cloned().flatten(),
        Some(UserId::new("alice"))
    );
}

/// The spawn model's `ModelChanged` is the persisted record: replaying the
/// session's own events rebinds it, over the agent's pin.
#[tokio::test]
async fn replay_keeps_the_spawn_model() {
    let (start, switched) = (seen(), seen());
    let cfg = config(
        &start,
        &switched,
        registry([
            profile("general", None),
            profile("plan", Some(("anthropic", "claude-x"))),
        ]),
    );
    let holly = Holly::spawn(cfg.clone());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(spawn(&sid, "plan", Some(ModelChoice::new("zai", "glm-b"))))
        .await
        .unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;
    let records: Vec<(Option<InMsg>, OutEvent)> = evs
        .into_iter()
        .filter(|e| e.session() == Some(&sid))
        .map(|e| (None, e))
        .collect();

    let session =
        entanglement_core::session::Session::replay(&records, &cfg, &sid).expect("replay");
    assert_eq!(session.model.as_deref(), Some("glm-b"));
    assert_eq!(session.provider.as_deref(), Some("zai"));
}
