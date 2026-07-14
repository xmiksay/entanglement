//! Realtime model/provider switch (#218): an `InMsg::SetModel` must re-resolve
//! against [`EngineConfig::model_resolver`], rebuild the session's backend, and
//! retarget the request model + generation with no engine restart. A switch with
//! no resolver wired surfaces an `Error` rather than silently doing nothing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, GenerationParams, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, ModelResolver, OutEvent, ResolvedModel, SessionId,
};

/// One request's observable inputs: the effective model id and generation knobs.
type Seen = Arc<Mutex<Vec<(Option<String>, Option<GenerationParams>)>>>;

/// Records the `(model, generation)` of every request, then ends the turn so the
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
            .push((req.model.map(str::to_string), req.generation));
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

/// A resolver that binds `(anthropic, claude-x)` to a fresh recording backend
/// with a distinct model + generation, so the test can prove the switch retargets
/// both. Any other pair is an error, mirroring an unknown provider.
fn switch_resolver(seen: &Seen) -> ModelResolver {
    let seen = seen.clone();
    Arc::new(move |provider: &str, model: &str| {
        if provider == "anthropic" && model == "claude-x" {
            Ok(ResolvedModel {
                provider: provider.to_string(),
                model: model.to_string(),
                llm_factory: recording_factory(&seen),
                generation: Some(GenerationParams {
                    temperature: None,
                    max_output_tokens: Some(8192),
                    thinking_budget_tokens: Some(4096),
                }),
                context_window: Some(200_000),
            })
        } else {
            Err(format!("unknown provider `{provider}`"))
        }
    })
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
async fn set_model_rebinds_backend_and_retargets_requests() {
    let start_seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let switched_seen: Seen = Arc::new(Mutex::new(Vec::new()));

    let cfg = EngineConfig {
        llm_factory: recording_factory(&start_seen),
        default_model: Some("glm-start".into()),
        generation: Some(GenerationParams {
            temperature: Some(0.2),
            max_output_tokens: Some(1024),
            thinking_budget_tokens: None,
        }),
        model_resolver: Some(switch_resolver(&switched_seen)),
        ..EngineConfig::default()
    };

    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    // First turn runs under the startup backend (no per-request model pin, the
    // profile has none) with the startup generation.
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "one".into(),
        })
        .await
        .unwrap();
    recv_until(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;

    // Switch to a different provider/model mid-session.
    holly
        .send(InMsg::SetModel {
            session: sid.clone(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
        })
        .await
        .unwrap();
    let changed = recv_until(&mut sub, |e| matches!(e, OutEvent::ModelChanged { .. })).await;
    match changed {
        OutEvent::ModelChanged {
            provider,
            model,
            context_window,
            ..
        } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(model, "claude-x");
            assert_eq!(context_window, Some(200_000));
        }
        _ => unreachable!(),
    }

    // Second turn must hit the *switched* backend, naming the new model and
    // carrying the resolved generation for that model.
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "two".into(),
        })
        .await
        .unwrap();
    recv_until(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;

    let start = start_seen.lock().unwrap().clone();
    assert_eq!(
        start.len(),
        1,
        "only the first turn hits the startup backend"
    );
    // No profile-pinned model, so the first request leaves the model unset (the
    // startup backend's own default) and carries the startup generation.
    assert_eq!(start[0].0, None);
    assert_eq!(start[0].1.and_then(|g| g.max_output_tokens), Some(1024));

    let switched = switched_seen.lock().unwrap().clone();
    assert_eq!(
        switched.len(),
        1,
        "the second turn hits the switched backend"
    );
    assert_eq!(switched[0].0.as_deref(), Some("claude-x"));
    assert_eq!(switched[0].1.and_then(|g| g.max_output_tokens), Some(8192));
    assert_eq!(
        switched[0].1.and_then(|g| g.thinking_budget_tokens),
        Some(4096)
    );
}

#[tokio::test]
async fn set_model_without_resolver_surfaces_error() {
    // No `model_resolver` wired: the switch can't resolve anything, so it must
    // emit an `Error` rather than silently no-op (the EchoLlm / bare-embedder path).
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::SetModel {
            session: sid.clone(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
        })
        .await
        .unwrap();

    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Error { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::Error { message, .. } = ev else {
        unreachable!()
    };
    assert!(
        message.contains("not supported"),
        "expected an unsupported-switch error, got {message:?}"
    );
}

#[tokio::test]
async fn set_model_unknown_provider_surfaces_resolver_error() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = EngineConfig {
        model_resolver: Some(switch_resolver(&seen)),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();

    holly
        .send(InMsg::SetModel {
            session: sid.clone(),
            provider: "nope".into(),
            model: "x".into(),
        })
        .await
        .unwrap();

    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Error { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::Error { message, .. } = ev else {
        unreachable!()
    };
    assert!(
        message.contains("cannot switch model") && message.contains("unknown provider"),
        "expected the resolver's error surfaced, got {message:?}"
    );
}
