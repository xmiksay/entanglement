//! Per-turn dynamic system prompt (#310, ADR-0078).
//!
//! `EngineConfig.system_prompt_resolver` lets an embedder whose prompt is
//! user-editable content — a site serving its prompt from a CMS page — override
//! the profile's `system_prompt` for a given turn, so an edit lands on the
//! *next* turn with no engine respawn. These tests assert:
//!
//! * mutating the resolver's backing store changes the prompt on the **next
//!   turn**, no respawn;
//! * an absent resolver leaves behaviour unchanged (the profile's own prompt);
//! * a `None` return from a present resolver falls back to the profile prompt.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    SessionId,
};

/// Per-session log of the system prompt seen in each request (arrival order).
type SeenBySession = Arc<Mutex<HashMap<String, Vec<String>>>>;

/// An LLM that records the system prompt of each request, then replies with
/// plain text so the turn ends at once.
struct RecordingLlm {
    session: String,
    seen: SeenBySession,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen
            .lock()
            .unwrap()
            .entry(self.session.clone())
            .or_default()
            .push(req.system.to_string());
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

/// Poll until session `sid` has recorded at least `n` requests, returning them.
async fn recorded_at_least(seen: &SeenBySession, sid: &str, n: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(reqs) = seen.lock().unwrap().get(sid) {
            if reqs.len() >= n {
                return reqs.clone();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("session `{sid}` recorded fewer than {n} requests");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn recording_config(seen: &SeenBySession) -> EngineConfig {
    let seen_factory = seen.clone();
    EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                session: "s".into(),
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    }
}

/// Acceptance: two turns with the resolver's value changed between them — the
/// second turn uses the new prompt, no engine respawn. Mirrors the documented
/// snapshot cache: an `Arc<RwLock<..>>` the embedder rehydrates from its store.
#[tokio::test]
async fn changing_prompt_takes_effect_next_turn() {
    let seen: SeenBySession = Arc::new(Mutex::new(HashMap::new()));
    let cache: Arc<RwLock<String>> = Arc::new(RwLock::new("PROMPT ONE".into()));
    let cache_resolver = cache.clone();

    let mut cfg = recording_config(&seen);
    cfg.system_prompt_resolver = Some(Arc::new(move |_sid, _profile| {
        Some(cache_resolver.read().unwrap().clone())
    }));

    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s");

    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    let after_first = recorded_at_least(&seen, "s", 1).await;
    assert_eq!(after_first[0], "PROMPT ONE");

    // Rehydrate the embedder's snapshot; the very next turn must reflect it.
    *cache.write().unwrap() = "PROMPT TWO".into();

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    let after_second = recorded_at_least(&seen, "s", 2).await;
    assert_eq!(
        after_second[1], "PROMPT TWO",
        "second turn should see the edited prompt without a respawn"
    );
}

/// Absent resolver ⇒ behaviour unchanged: the request carries the active
/// profile's own `system_prompt` (the built-in `build` profile's).
#[tokio::test]
async fn absent_resolver_uses_profile_prompt() {
    let seen: SeenBySession = Arc::new(Mutex::new(HashMap::new()));
    let cfg = recording_config(&seen);
    let profile_prompt = cfg.profiles.get("build").unwrap().system_prompt.clone();

    let holly = Holly::spawn(cfg);
    holly
        .send(InMsg::prompt(SessionId::new("s"), "go"))
        .await
        .unwrap();

    let reqs = recorded_at_least(&seen, "s", 1).await;
    assert_eq!(reqs[0], profile_prompt);
}

/// A present resolver that returns `None` for the turn falls back to the
/// profile's own prompt — the override is opt-in per turn.
#[tokio::test]
async fn none_return_falls_back_to_profile_prompt() {
    let seen: SeenBySession = Arc::new(Mutex::new(HashMap::new()));
    let mut cfg = recording_config(&seen);
    let profile_prompt = cfg.profiles.get("build").unwrap().system_prompt.clone();
    cfg.system_prompt_resolver = Some(Arc::new(|_sid, _profile| None));

    let holly = Holly::spawn(cfg);
    holly
        .send(InMsg::prompt(SessionId::new("s"), "go"))
        .await
        .unwrap();

    let reqs = recorded_at_least(&seen, "s", 1).await;
    assert_eq!(reqs[0], profile_prompt);
}
