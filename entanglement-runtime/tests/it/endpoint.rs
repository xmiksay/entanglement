//! Definition-driven HTTP endpoint tools (#560 P8), end-to-end: a real
//! `EndpointTool` calling a local mock server through the shared
//! `entanglement_core::HttpClient` pool, driven through the actual tool
//! executor so the 32 KiB response cap and P4's pre-dispatch schema
//! validation are both exercised for real — not just unit-tested in
//! isolation. Mirrors `mcp_http.rs`'s mock-server pattern (axum on a loopback
//! `TcpListener`); needs the `serve` feature for axum, same gate that file
//! uses.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, HttpClient, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::endpoint::{EndpointConfig, EndpointTool};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;
use tokio::net::TcpListener;

fn ensure_shared_state_disabled() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        std::env::set_var("ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE", "1");
    });
}

fn test_http_client() -> HttpClient {
    ensure_shared_state_disabled();
    HttpClient::new().unwrap()
}

async fn ok_body(State(body): State<&'static str>) -> impl IntoResponse {
    body
}

/// Spawn a one-route GET server answering with `body`, returning its base URL.
async fn spawn_server(path: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(path, get(ok_body)).with_state(body);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}")
}

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
}
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = {
            let mut responses = self.responses.lock().unwrap();
            responses.pop().unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            })
        };
        Ok(stream_from_response(resp))
    }
}

async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() == Some(sid) {
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                break;
            }
        }
    }
    out
}

fn tool_outputs(events: &[OutEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// Register `tool` under the default-allow `build` profile and drive one
/// `ToolCall` against it through the real executor, returning every
/// `ToolOutput` in call order — mirrors `arg_validate.rs`'s `run_calls`.
async fn run_call(tool: EndpointTool, name: &str, input: &str) -> Vec<(String, bool)> {
    let responses = vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: name.into(),
                input: input.into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        },
    ];
    let scripted = Arc::new(responses);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(tool);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    tool_outputs(&collect(sub, &sid).await)
}

#[tokio::test]
async fn endpoint_tool_calls_a_real_server_end_to_end() {
    let base = spawn_server("/weather", "sunny, 20C").await;
    let cfg: EndpointConfig = serde_yaml::from_str(&format!(
        "url: \"{base}/weather\"\ndescription: weather lookup"
    ))
    .unwrap();
    let tool = EndpointTool::new("endpoint__weather".to_string(), &cfg, test_http_client());

    let outputs = run_call(tool, "endpoint__weather", "{}").await;
    let (output, is_error) = &outputs[0];
    assert!(!is_error, "{output}");
    assert!(output.contains("HTTP 200"), "{output}");
    assert!(output.contains("sunny, 20C"), "{output}");
}

#[tokio::test]
async fn endpoint_tool_substitutes_a_url_param_from_call_args() {
    let base = spawn_server("/weather/ny", "ny weather").await;
    let cfg: EndpointConfig = serde_yaml::from_str(&format!(
        "url: \"{base}/weather/{{{{city}}}}\"\nparams:\n  city: {{ type: string }}"
    ))
    .unwrap();
    let tool = EndpointTool::new("endpoint__weather".to_string(), &cfg, test_http_client());

    let outputs = run_call(tool, "endpoint__weather", r#"{"city":"ny"}"#).await;
    let (output, is_error) = &outputs[0];
    assert!(!is_error, "{output}");
    assert!(output.contains("ny weather"), "{output}");
}

#[tokio::test]
async fn endpoint_response_over_32kib_is_truncated_with_a_marker() {
    // Leak the oversized body as a `&'static str` — the mock server needs a
    // `'static` body for its `axum::extract::State`, and this is a one-shot
    // test process, so the leak is harmless.
    let big: &'static str = Box::leak(("x".repeat(40 * 1024)).into_boxed_str());
    let base = spawn_server("/big", big).await;
    let cfg: EndpointConfig = serde_yaml::from_str(&format!("url: \"{base}/big\"")).unwrap();
    let tool = EndpointTool::new("endpoint__big".to_string(), &cfg, test_http_client());

    let outputs = run_call(tool, "endpoint__big", "{}").await;
    let (output, is_error) = &outputs[0];
    assert!(
        !is_error,
        "a large response is a normal result, not an error: {output}"
    );
    assert!(
        output.contains("[truncated: response exceeded 32 KiB]"),
        "{}",
        output.len()
    );
    assert!(
        output.len() < 40 * 1024,
        "output must actually be capped, got {} bytes",
        output.len()
    );
}

#[tokio::test]
async fn missing_required_param_declines_before_the_endpoint_is_ever_called() {
    // P4 integration (ADR-0196 §6): pre-dispatch schema validation must
    // decline a bad call before `EndpointTool::run` — and therefore the real
    // HTTP request — ever happens. No mock server is even spawned here: if
    // the call incorrectly reached the network, there'd be nothing to answer
    // it and the test would hang/fail on a connection error instead of
    // asserting the schema decline text.
    let cfg: EndpointConfig = serde_yaml::from_str(
        "url: \"http://127.0.0.1:1/unused\"\nparams:\n  city: { type: string, required: true }",
    )
    .unwrap();
    let tool = EndpointTool::new("endpoint__weather".to_string(), &cfg, test_http_client());

    let outputs = run_call(tool, "endpoint__weather", "{}").await;
    let (output, is_error) = &outputs[0];
    assert!(*is_error, "schema violation must set is_error: {output}");
    assert!(output.contains("missing required: city"), "{output}");
    assert!(output.contains("correct usage"), "{output}");
}
