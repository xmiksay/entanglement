//! Per-agent-profile persisted generation-parameter overlay (#374, ADR-0094),
//! applied at session start through `EngineConfig::generation_resolver` — the
//! generation-parameter analogue of the model pin's `model_resolver` seam
//! (#323, ADR-0081). Precedence: per-session memory (a live `SetGeneration`
//! made under a profile — relevant only across a resume now, ADR-0207 §9
//! retired the live agent switch that used to re-apply it) > the resolver's
//! persisted value > keep the current binding.
//!
//! The scaffolding mirrors `agent_model_pin.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentProfile, EngineConfig, GenerationParams, GenerationResolver, Holly,
    InMsg, Llm, LlmRequest, LlmResponse, LlmStream, OutEvent, ProfileRegistry, SessionId,
};

/// Every request's effective generation knobs, in order.
type Seen = Arc<Mutex<Vec<Option<GenerationParams>>>>;

struct RecordingLlm {
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.generation);
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

/// A resolver over a fixed map of profile name → persisted generation params.
fn resolver(overrides: &[(&str, GenerationParams)]) -> GenerationResolver {
    let map: std::collections::HashMap<String, GenerationParams> =
        overrides.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    Arc::new(move |name: &str| map.get(name).copied())
}

fn profile(name: &str) -> AgentProfile {
    AgentProfile {
        name: name.to_string(),
        description: String::new(),
        system_prompt: String::new(),
        model: None,
        provider: None,
    }
}

fn registry(profiles: impl IntoIterator<Item = AgentProfile>) -> ProfileRegistry {
    let mut reg = ProfileRegistry::default();
    for p in profiles {
        reg.insert(p);
    }
    reg
}

fn config(seen: &Seen, profiles: ProfileRegistry, resolver: GenerationResolver) -> EngineConfig {
    EngineConfig {
        llm_factory: recording_factory(seen),
        profiles,
        generation_resolver: Some(resolver),
        ..EngineConfig::default()
    }
}

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

fn params(temp: f32) -> GenerationParams {
    GenerationParams {
        temperature: Some(temp),
        max_output_tokens: None,
        thinking_budget_tokens: None,
        reasoning_effort: None,
    }
}

/// A pinned profile's persisted override applies when a session spawns
/// directly under it (ADR-0207 §9: an agent is chosen once, at spawn — there
/// is no live `SetAgent` switch to apply it on any more).
#[tokio::test]
async fn persisted_override_applies_at_spawn() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([profile("general"), profile("plan")]);
    let holly = Holly::spawn(config(&seen, profiles, resolver(&[("plan", params(0.9))])));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "plan".into(),
            prompt: String::new(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let evs = drain_until(&mut sub, |e| {
        matches!(e, OutEvent::GenerationChanged { .. })
    })
    .await;
    match evs.last().unwrap() {
        OutEvent::GenerationChanged { generation, .. } => {
            assert_eq!(generation.temperature, Some(0.9));
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn profile_without_override_emits_no_generation_changed_at_spawn() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([profile("general"), profile("other")]);
    let holly = Holly::spawn(config(&seen, profiles, resolver(&[])));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "other".into(),
            prompt: String::new(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let evs = drain_until(
        &mut sub,
        |e| matches!(e, OutEvent::AgentChanged { agent, .. } if agent == "other"),
    )
    .await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e, OutEvent::GenerationChanged { .. })),
        "a profile with no persisted override must not emit GenerationChanged"
    );
}

#[tokio::test]
async fn session_start_applies_the_persisted_override() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let profiles = registry([profile("general")]);
    let holly = Holly::spawn(config(
        &seen,
        profiles,
        resolver(&[("general", params(0.42))]),
    ));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // The very first prompt spins up the session, applying the start override
    // (GenerationChanged) before the turn runs.
    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    let evs = drain_until(&mut sub, is_done(&sid)).await;
    assert!(evs.iter().any(
        |e| matches!(e, OutEvent::GenerationChanged { generation, .. } if generation.temperature == Some(0.42))
    ));
}

#[test]
fn replay_reconstructs_generation_and_profile_generation() {
    let profiles = registry([profile("general"), profile("plan")]);
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = config(&seen, profiles, resolver(&[]));
    let sid = SessionId::new("s1");

    let records: Vec<(Option<InMsg>, OutEvent)> = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                profile: "general".into(),
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
            OutEvent::GenerationChanged {
                session: sid.clone(),
                generation: params(0.55),
            },
        ),
    ];

    let session =
        entanglement_core::session::Session::replay(&records, &cfg, &sid).expect("replay");
    assert_eq!(session.generation, Some(params(0.55)));
    assert_eq!(session.profile.name, "plan");
    assert_eq!(session.profile_generation.get("plan"), Some(&params(0.55)));
}

#[test]
fn replay_a_later_model_changed_still_wins_generation_stays() {
    // A `GenerationChanged` followed by a `ModelChanged` in the log: the model
    // switch is a separate concern (no resolver wired here, so it just warns and
    // keeps the prior generation) — generation reconstruction is unaffected by
    // an interleaved model switch.
    let profiles = registry([profile("general")]);
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = config(&seen, profiles, resolver(&[]));
    let sid = SessionId::new("s1");

    let records: Vec<(Option<InMsg>, OutEvent)> = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                profile: "general".into(),
                model: None,
                root: true,
                ts: 0,
                user: None,
                sponsored: false,
            },
        ),
        (
            None,
            OutEvent::GenerationChanged {
                session: sid.clone(),
                generation: params(0.3),
            },
        ),
    ];

    let session =
        entanglement_core::session::Session::replay(&records, &cfg, &sid).expect("replay");
    assert_eq!(session.generation, Some(params(0.3)));
    assert_eq!(
        session.profile_generation.get("general"),
        Some(&params(0.3))
    );
}
