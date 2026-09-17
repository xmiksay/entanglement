//! Per-agent-profile provider/model pinning (#323, ADR-0081). A profile pins
//! its own `(provider, model)`; spawning under it — at session start, or on
//! replay — re-binds the session's backend through the same `model_resolver`
//! seam a live `/model` (`SetModel`) switch uses (#218). ADR-0207 §9 retired
//! the live `SetAgent` switch this used to also apply on, so the only
//! remaining "rebind" moments are spawn-time and replay.
//!
//! The scaffolding mirrors `model_switch.rs`: a recording backend captures each
//! request's effective model, and a resolver maps a fixed set of `(provider,
//! model)` pairs to fresh recording backends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, ModelResolver, OutEvent, ProfileRegistry, ResolvedModel, SessionId,
};

/// Every request's effective model id (`req.model`), in order.
type Seen = Arc<Mutex<Vec<Option<String>>>>;

/// Records the effective model of every request, then ends the turn so the
/// session returns to idle and can accept the next command.
struct RecordingLlm {
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen
            .lock()
            .unwrap()
            .push(req.model.map(str::to_string));
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

fn recording_factory(seen: &Seen) -> entanglement_core::LlmFactory {
    let seen = seen.clone();
    Arc::new(move || Box::new(RecordingLlm { seen: seen.clone() }) as Box<dyn Llm>)
}

/// A resolver over a fixed set of known `(provider, model)` pairs — every one
/// binds a fresh recording backend on the *same* shared `seen`, so a switched
/// request is observable by its `req.model`. Any unknown pair errors, mirroring
/// an unknown provider / missing key.
fn resolver(seen: &Seen) -> ModelResolver {
    let seen = seen.clone();
    Arc::new(move |_user, provider: &str, model: &str| {
        let known = matches!(
            (provider, model),
            ("anthropic", "claude-x") | ("zai", "glm-b") | ("zai", "glm-c")
        );
        if !known {
            return Err(format!("unknown provider `{provider}`"));
        }
        Ok(ResolvedModel {
            provider: provider.to_string(),
            model: model.to_string(),
            llm_factory: recording_factory(&seen),
            generation: None,
            context_window: Some(100_000),
        })
    })
}

fn profile(name: &str, pin: Option<(&str, &str)>) -> AgentProfile {
    AgentProfile {
        name: name.to_string(),
        description: String::new(),
        system_prompt: String::new(),
        model: pin.map(|(_, m)| m.to_string()),
        provider: pin.map(|(p, _)| p.to_string()),
    }
}

/// A model-only profile (legacy request-level fallback, no provider pin).
fn model_only_profile(name: &str, model: &str) -> AgentProfile {
    let mut p = profile(name, None);
    p.model = Some(model.to_string());
    p
}

fn registry(profiles: impl IntoIterator<Item = AgentProfile>) -> ProfileRegistry {
    let mut reg = ProfileRegistry::default();
    for p in profiles {
        reg.insert(p);
    }
    reg
}

fn config(seen: &Seen, switch_seen: &Seen, profiles: ProfileRegistry) -> EngineConfig {
    EngineConfig {
        llm_factory: recording_factory(seen),
        profiles,
        model_resolver: Some(resolver(switch_seen)),
        ..EngineConfig::default()
    }
}

/// Collect every event up to and including the first one matching `pred`.
async fn drain_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) => {
                let hit = pred(&ev);
                out.push(ev);
                if hit {
                    return out;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

fn is_done(session: &SessionId) -> impl Fn(&OutEvent) -> bool + '_ {
    move |e| matches!(e, OutEvent::Done { session: s, .. } if s == session)
}

#[tokio::test]
async fn pinned_profile_rebinds_at_spawn() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("general", None),
        profile("plan", Some(("anthropic", "claude-x"))),
    ]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // Spawn straight under the pinned `plan`: AgentChanged then ModelChanged,
    // in that order, before the turn runs (ADR-0207 §9 — no live switch).
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "plan".into(),
            prompt: "one".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;
    let agent_idx = evs
        .iter()
        .position(|e| matches!(e, OutEvent::AgentChanged { agent, .. } if agent == "plan"));
    let model_idx = evs
        .iter()
        .position(|e| matches!(e, OutEvent::ModelChanged { .. }));
    assert!(
        agent_idx.unwrap() < model_idx.unwrap(),
        "AgentChanged precedes ModelChanged"
    );
    match evs
        .iter()
        .find(|e| matches!(e, OutEvent::ModelChanged { .. }))
        .unwrap()
    {
        OutEvent::ModelChanged {
            provider, model, ..
        } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(model, "claude-x");
        }
        _ => unreachable!(),
    }

    assert!(
        start.lock().unwrap().is_empty(),
        "startup backend never used"
    );
    assert_eq!(
        switched.lock().unwrap().clone(),
        vec![Some("claude-x".into())]
    );
}

/// Session-start pin application is best-effort (unlike the old live
/// `SetAgent` handler it replaced, ADR-0207 §9): a resolver failure just
/// warns and keeps the startup default — no `OutEvent::Error`, matching
/// replay's stance.
#[tokio::test]
async fn resolver_error_at_spawn_keeps_default_binding() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("general", None),
        profile("bad", Some(("nope", "x"))), // resolver rejects this pair
    ]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "bad".into(),
            prompt: "one".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, OutEvent::AgentChanged { agent, .. } if agent == "bad")),
        "the spawn still succeeds under `bad`"
    );
    assert!(
        !evs.iter().any(|e| matches!(e, OutEvent::Error { .. })),
        "best-effort at session start: no Error surfaces"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, OutEvent::ModelChanged { .. })),
        "the failed pin never rebinds"
    );

    assert_eq!(
        start.lock().unwrap().len(),
        1,
        "the turn hit the startup backend"
    );
    assert!(
        switched.lock().unwrap().is_empty(),
        "resolver error must not build the switched backend"
    );
}

#[tokio::test]
async fn session_start_applies_the_pin() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    // The default `general` profile itself carries a pin.
    let profiles = registry([profile("general", Some(("anthropic", "claude-x")))]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // The very first prompt spins up the session, which applies the start pin
    // (ModelChanged) before the turn runs.
    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, OutEvent::ModelChanged { model, .. } if model == "claude-x")),
        "session start emits ModelChanged for the pin"
    );
    // The first turn already ran under the pinned model.
    assert!(
        start.lock().unwrap().is_empty(),
        "startup backend never used"
    );
    assert_eq!(
        switched.lock().unwrap().clone(),
        vec![Some("claude-x".into())]
    );
}

#[tokio::test]
async fn model_only_pin_stays_request_level() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("general", None),
        model_only_profile("legacy", "glm-legacy"),
    ]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // A model-only profile has no pin: spawning under it emits no
    // `ModelChanged`, so the model rides the request as a fallback on the
    // *unchanged* startup backend.
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "legacy".into(),
            prompt: "one".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e, OutEvent::ModelChanged { .. })),
        "a model-only profile must not rebind"
    );

    // The turn ran on the startup backend, carrying the model-only fallback
    // as `req.model` (never a rebind onto the switched backend).
    assert_eq!(
        start.lock().unwrap().clone(),
        vec![Some("glm-legacy".into())]
    );
    assert!(switched.lock().unwrap().is_empty());
}

#[test]
fn replay_rebinds_and_reconstructs_the_profile() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("general", None),
        profile("plan", Some(("anthropic", "claude-x"))),
    ]);
    let cfg = config(&start, &switched, profiles);
    let sid = SessionId::new("s1");

    // A log: started under `plan` (its own `AgentChanged` confirms it), then
    // switched model to `glm-b` live.
    let records: Vec<(Option<InMsg>, OutEvent)> = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                profile: "plan".into(),
                model: None,
                root: true,
                ts: 0,
                user: None,
                sponsored: false,
            },
        ),
        (
            None,
            OutEvent::AgentChanged {
                session: sid.clone(),
                agent: "plan".into(),
            },
        ),
        (
            None,
            OutEvent::ModelChanged {
                session: sid.clone(),
                provider: "zai".into(),
                model: "glm-b".into(),
                context_window: Some(100_000),
            },
        ),
    ];

    let session =
        entanglement_core::session::Session::replay(&records, &cfg, &sid).expect("replay");
    // Re-bound to the switched model, tracking the resolved provider — wins
    // over `plan`'s own static pin (`claude-x`), the same live-choice-wins
    // precedence `SetModel` has.
    assert_eq!(session.model.as_deref(), Some("glm-b"));
    assert_eq!(session.provider.as_deref(), Some("zai"));
    assert_eq!(session.profile.name, "plan");
}
