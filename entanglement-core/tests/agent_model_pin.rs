//! Per-agent-profile provider/model pinning + rebind on `SetAgent` (#323,
//! ADR-0081). A profile pins its own `(provider, model)`; switching to it — via
//! `SetAgent`, at session start, or on replay — re-binds the session's backend
//! through the same `model_resolver` seam a live `/model` (`SetModel`) switch
//! uses (#218). Precedence: per-session memory (a `/model` choice made under a
//! profile) > the profile's static pin > keep the current binding.
//!
//! The scaffolding mirrors `model_switch.rs`: a recording backend captures each
//! request's effective model, and a resolver maps a fixed set of `(provider,
//! model)` pairs to fresh recording backends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, ModelResolver, OutEvent, Permission, PermissionProfile,
    ProfileRegistry, ResolvedModel, SessionId,
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
    Arc::new(move |provider: &str, model: &str| {
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
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: pin.map(|(_, m)| m.to_string()),
        provider: pin.map(|(p, _)| p.to_string()),
        permission: PermissionProfile::new(Permission::Allow),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
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
async fn pinned_profile_rebinds_on_set_agent() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("build", None),
        profile("plan", Some(("anthropic", "claude-x"))),
    ]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // First turn under pin-less `build` → startup backend, no model pin.
    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    // Switch to the pinned `plan`: AgentChanged then ModelChanged, in that order.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    let evs = drain_until(&mut sub, |e| matches!(e, OutEvent::ModelChanged { .. })).await;
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
    match evs.last().unwrap() {
        OutEvent::ModelChanged {
            provider, model, ..
        } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(model, "claude-x");
        }
        _ => unreachable!(),
    }

    // Next turn hits the switched backend, naming the pinned model.
    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    assert_eq!(start.lock().unwrap().clone(), vec![None]);
    assert_eq!(
        switched.lock().unwrap().clone(),
        vec![Some("claude-x".into())]
    );
}

#[tokio::test]
async fn pinless_profile_keeps_live_override() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([profile("build", None), profile("coder", None)]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    // Live `/model` override under pin-less `build`.
    holly
        .send(InMsg::SetModel {
            session: sid.clone(),
            provider: "zai".into(),
            model: "glm-b".into(),
        })
        .await
        .unwrap();
    drain_until(&mut sub, |e| matches!(e, OutEvent::ModelChanged { .. })).await;

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    // Switch to another pin-less profile with no memory: no ModelChanged, the
    // override survives.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "coder".into(),
        })
        .await
        .unwrap();
    let evs = drain_until(
        &mut sub,
        |e| matches!(e, OutEvent::AgentChanged { agent, .. } if agent == "coder"),
    )
    .await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e, OutEvent::ModelChanged { .. })),
        "a pin-less profile with no memory must not rebind"
    );

    holly
        .send(InMsg::prompt(sid.clone(), "three"))
        .await
        .unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    // The startup backend was used once; both post-override turns — including the
    // one after switching to a pin-less profile — kept the live override.
    assert_eq!(start.lock().unwrap().clone(), vec![None]);
    assert_eq!(
        switched.lock().unwrap().clone(),
        vec![Some("glm-b".into()), Some("glm-b".into())]
    );
}

#[tokio::test]
async fn session_memory_wins_over_static_pin_on_switch_back() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("build", None),
        profile("plan", Some(("anthropic", "claude-x"))),
        profile("other", Some(("zai", "glm-c"))),
    ]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // Under `plan`, a live `/model` choice records session memory for `plan`.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    drain_until(&mut sub, |e| matches!(e, OutEvent::ModelChanged { .. })).await;
    holly
        .send(InMsg::SetModel {
            session: sid.clone(),
            provider: "zai".into(),
            model: "glm-b".into(),
        })
        .await
        .unwrap();
    drain_until(&mut sub, |e| matches!(e, OutEvent::ModelChanged { .. })).await;

    // Switch away (to a differently-pinned profile), then back to `plan`.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "other".into(),
        })
        .await
        .unwrap();
    drain_until(
        &mut sub,
        |e| matches!(e, OutEvent::ModelChanged { model, .. } if model == "glm-c"),
    )
    .await;
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    let evs = drain_until(&mut sub, |e| matches!(e, OutEvent::ModelChanged { .. })).await;
    // Memory (`glm-b`) wins over `plan`'s static pin (`claude-x`).
    match evs.last().unwrap() {
        OutEvent::ModelChanged { model, .. } => assert_eq!(model, "glm-b"),
        _ => unreachable!(),
    }

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;
    assert_eq!(
        switched.lock().unwrap().last().unwrap().clone(),
        Some("glm-b".into())
    );
}

#[tokio::test]
async fn resolver_error_keeps_binding_but_switches_agent() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("build", None),
        profile("bad", Some(("nope", "x"))), // resolver rejects this pair
    ]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    // The agent switch still succeeds; the failed pin surfaces the same Error as
    // SetModel and keeps the old binding.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "bad".into(),
        })
        .await
        .unwrap();
    let evs = drain_until(
        &mut sub,
        |e| matches!(e, OutEvent::Error { session, .. } if *session == sid),
    )
    .await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, OutEvent::AgentChanged { agent, .. } if agent == "bad")),
        "AgentChanged still succeeds"
    );
    match evs.last().unwrap() {
        OutEvent::Error { message, .. } => assert!(message.contains("cannot switch model")),
        _ => unreachable!(),
    }

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;
    // Still on the startup backend — the failed pin did not rebind (the switched
    // backend was never built). The profile's model rides the request as a
    // request-level fallback, but that is not a backend rebind.
    assert_eq!(
        start.lock().unwrap().len(),
        2,
        "both turns hit the startup backend"
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
    // The default `build` profile itself carries a pin.
    let profiles = registry([profile("build", Some(("anthropic", "claude-x")))]);
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
        profile("build", None),
        model_only_profile("legacy", "glm-legacy"),
    ]);
    let holly = Holly::spawn(config(&start, &switched, profiles));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    // A model-only profile has no pin: SetAgent emits no ModelChanged, so the
    // model rides the request as a fallback on the *unchanged* startup backend.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "legacy".into(),
        })
        .await
        .unwrap();
    let evs = drain_until(
        &mut sub,
        |e| matches!(e, OutEvent::AgentChanged { agent, .. } if agent == "legacy"),
    )
    .await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e, OutEvent::ModelChanged { .. })),
        "a model-only profile must not rebind"
    );

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    drain_until(&mut sub, is_done(&sid)).await;

    // Both turns ran on the startup backend; the second carried the model-only
    // fallback as `req.model` (never a rebind onto the switched backend).
    assert_eq!(
        start.lock().unwrap().clone(),
        vec![None, Some("glm-legacy".into())]
    );
    assert!(switched.lock().unwrap().is_empty());
}

#[test]
fn replay_rebinds_and_reconstructs_memory() {
    let start: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([
        profile("build", None),
        profile("plan", Some(("anthropic", "claude-x"))),
    ]);
    let cfg = config(&start, &switched, profiles);
    let sid = SessionId::new("s1");

    // A log: started under build, switched agent to plan, switched model to glm-b.
    let records: Vec<(Option<InMsg>, OutEvent)> = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                profile: "build".into(),
                model: None,
                root: true,
                ts: 0,
            },
        ),
        (
            None,
            OutEvent::AgentChanged {
                session: sid.clone(),
                agent: "plan".into(),
                profile_detail: None,
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
    // Re-bound to the switched model, tracking the resolved provider.
    assert_eq!(session.model.as_deref(), Some("glm-b"));
    assert_eq!(session.provider.as_deref(), Some("zai"));
    assert_eq!(session.profile.name, "plan");
    // Per-profile memory reconstructed from the ModelChanged record.
    assert_eq!(
        session.profile_models.get("plan"),
        Some(&("zai".to_string(), "glm-b".to_string()))
    );
}
