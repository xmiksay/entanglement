//! Generation-parameter channel (#191): the knobs resolved on
//! [`EngineConfig::generation`] must reach the backend on every
//! [`LlmRequest`], and a `None` must leave the request's `generation` unset so
//! the client falls back to its own defaults.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, GenerationParams, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, SessionId,
};

/// Records the `generation` payload of every request, then ends the turn.
struct RecordingLlm {
    seen: Arc<Mutex<Vec<Option<GenerationParams>>>>,
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

fn config_with(
    generation: Option<GenerationParams>,
) -> (EngineConfig, Arc<Mutex<Vec<Option<GenerationParams>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                seen: recorder.clone(),
            }) as Box<dyn Llm>
        }),
        generation,
        ..EngineConfig::default()
    };
    (cfg, seen)
}

async fn first_recorded(
    seen: &Arc<Mutex<Vec<Option<GenerationParams>>>>,
) -> Option<GenerationParams> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(g) = seen.lock().unwrap().first().copied() {
            return g;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("no LLM request was recorded");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run_one_turn(cfg: EngineConfig) {
    let holly = Holly::spawn(cfg);
    holly
        .send(InMsg::prompt(SessionId::new("s1"), "go"))
        .await
        .unwrap();
}

#[tokio::test]
async fn resolved_generation_params_reach_the_request() {
    let params = GenerationParams {
        temperature: Some(0.4),
        max_output_tokens: Some(4096),
        thinking_budget_tokens: Some(2048),
        reasoning_effort: None,
    };
    let (cfg, seen) = config_with(Some(params));
    run_one_turn(cfg).await;
    assert_eq!(first_recorded(&seen).await, Some(params));
}

#[tokio::test]
async fn no_generation_config_leaves_request_knobs_unset() {
    let (cfg, seen) = config_with(None);
    run_one_turn(cfg).await;
    assert_eq!(first_recorded(&seen).await, None);
}
