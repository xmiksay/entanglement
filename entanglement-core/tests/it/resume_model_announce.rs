//! A resumed session must announce the RESOLVED model, not the starting
//! profile's static pin (log-proven bug): `session_loop` passed the static
//! `profile.model` straight into the resumed `SessionStarted`, even though
//! `Session::replay` had already rebound `s.model`/`s.provider` from the
//! log's last `ModelChanged` (ADR-0081 — session memory wins over the static
//! pin). That's the *binding*; the *announcement* missed the same treatment,
//! so a reconnecting head (in particular the TUI status bar, which updates
//! only on `ModelChanged`, never on `SessionStarted.model`) displayed the pin
//! while the session actually ran something else. The fix mirrors the
//! existing `predecessor`/`user`/`sponsored` resumed-takes-precedence
//! pattern and follows up with a corrective `ModelChanged` right after the
//! resumed `SessionStarted`, seeding every head exactly like a live switch
//! would (#218).
//!
//! Scaffolding mirrors `agent_model_pin.rs`: a recording backend + a
//! resolver over a fixed set of `(provider, model)` pairs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, ModelResolver, OutEvent, ProfileRegistry, ResolvedModel, SessionId,
};

type Seen = Arc<Mutex<Vec<Option<String>>>>;

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

/// Resolves the two pairs these tests use: `plan`'s static pin
/// (`anthropic`/`claude-x`) and the live override a resumed log diverges to
/// (`zai`/`glm-b`).
fn resolver(seen: &Seen) -> ModelResolver {
    let seen = seen.clone();
    Arc::new(move |_user, provider: &str, model: &str| {
        let known = matches!(
            (provider, model),
            ("anthropic", "claude-x") | ("zai", "glm-b")
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

fn registry() -> ProfileRegistry {
    let mut reg = ProfileRegistry::default();
    // `Session::replay` falls back to the default `general` profile in a
    // couple of edge cases (unrelated to what these tests exercise), so it
    // must exist alongside the pinned `plan` profile these tests actually use.
    reg.insert(profile("general", None));
    reg.insert(profile("plan", Some(("anthropic", "claude-x"))));
    reg
}

fn config(seen: &Seen) -> EngineConfig {
    EngineConfig {
        profiles: registry(),
        model_resolver: Some(resolver(seen)),
        ..EngineConfig::default()
    }
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

/// Collect every event for `sid` up to and including the first one matching
/// `pred`.
async fn drain_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    pred: impl Fn(&OutEvent) -> bool,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let ev = recv_until(sub, |e| e.session() == Some(sid)).await;
        let hit = pred(&ev);
        out.push(ev);
        if hit {
            return out;
        }
    }
}

/// A minimal resumed-session log: started under `plan` (whose static pin is
/// `anthropic`/`claude-x`), then a live `/model` override to `zai`/`glm-b` —
/// the session's actual binding a resumed process must reflect, not `plan`'s
/// static pin.
fn diverged_log(sid: &SessionId) -> Vec<(Option<InMsg>, OutEvent)> {
    vec![
        (
            None,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                profile: "plan".into(),
                model: Some("claude-x".into()),
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
    ]
}

#[tokio::test]
async fn resumed_session_announces_resolved_model_and_seeds_a_corrective_model_changed() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(config(&seen));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("resume-model-announce");

    holly.resume(sid.clone(), diverged_log(&sid)).await.unwrap();

    // SessionStarted must announce the resolved binding (glm-b), not `plan`'s
    // static pin (claude-x) — replay already rebound `s.model` before this
    // event is emitted.
    let started = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::SessionStarted { model, .. } = started else {
        unreachable!()
    };
    assert_eq!(
        model,
        Some("glm-b".into()),
        "resumed SessionStarted must announce the resolved model, not plan's static pin"
    );

    // A corrective ModelChanged follows to seed heads that only update on it
    // (the TUI status bar never parses SessionStarted.model).
    let changed = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ModelChanged { session, .. } if *session == sid),
    )
    .await;
    match changed {
        OutEvent::ModelChanged {
            provider,
            model,
            context_window,
            ..
        } => {
            assert_eq!(provider, "zai");
            assert_eq!(model, "glm-b");
            assert_eq!(context_window, Some(100_000));
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn fresh_session_start_is_unaffected() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(config(&seen));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("fresh-model-announce");

    // No resume — a fresh spawn under `plan`'s static pin.
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "plan".into(),
            prompt: "hi".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();

    let started = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::SessionStarted { model, .. } = started else {
        unreachable!()
    };
    assert_eq!(
        model,
        Some("claude-x".into()),
        "a fresh session's announced model is unchanged: the profile's static pin"
    );

    let evs = drain_until(&mut sub, &sid, |e| matches!(e, OutEvent::Done { .. })).await;
    let model_changed_count = evs
        .iter()
        .filter(|e| matches!(e, OutEvent::ModelChanged { .. }))
        .count();
    assert_eq!(
        model_changed_count, 1,
        "exactly one ModelChanged (the pin bind) — no spurious extra from the resumed corrective path"
    );
}

#[tokio::test]
async fn resuming_an_already_corrected_log_stays_idempotent() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let holly = Holly::spawn(config(&seen));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("resume-idempotent");

    // Simulate a log that already carries a prior resume's corrective
    // re-announce: the diverged log, plus the re-emitted SessionStarted +
    // ModelChanged this fix would have appended on an earlier resume.
    let mut log = diverged_log(&sid);
    log.push((
        None,
        OutEvent::SessionStarted {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            profile: "plan".into(),
            model: Some("glm-b".into()),
            root: true,
            ts: 1,
            user: None,
            sponsored: false,
        },
    ));
    log.push((
        None,
        OutEvent::ModelChanged {
            session: sid.clone(),
            provider: "zai".into(),
            model: "glm-b".into(),
            context_window: Some(100_000),
        },
    ));

    holly.resume(sid.clone(), log).await.unwrap();

    let started = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::SessionStarted { model, .. } = started else {
        unreachable!()
    };
    assert_eq!(
        model,
        Some("glm-b".into()),
        "resuming an already-corrected log still announces the resolved model"
    );

    let changed = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::ModelChanged { session, .. } if *session == sid),
    )
    .await;
    match changed {
        OutEvent::ModelChanged {
            provider, model, ..
        } => {
            assert_eq!(provider, "zai");
            assert_eq!(model, "glm-b");
        }
        _ => unreachable!(),
    }

    // No second ModelChanged follows: the corrective announce fires once per
    // resume (guarded by `s.model.is_some()` at loop entry), not once per
    // matching log entry — a log that already contains two ModelChanged
    // records must not grow the re-announce on every future resume.
    let extra = tokio::time::timeout(
        Duration::from_millis(300),
        recv_until(
            &mut sub,
            |e| matches!(e, OutEvent::ModelChanged { session, .. } if *session == sid),
        ),
    )
    .await;
    assert!(
        extra.is_err(),
        "expected no second ModelChanged; the corrective announce must fire exactly once per resume"
    );
}
