//! Physical per-agent tool restriction — enforcement half (#116, ADR-0038).
//!
//! Core withholds a masked tool's schema, but the runtime executor is the hard
//! boundary: even if the model hallucinates a masked `edit` call, the executor
//! refuses it *before* permission is resolved and the tool never runs. Here the
//! scripted LLM is forced to call `edit` under the read-only `explore` profile
//! (allowlist `read`/`glob`/`grep`), and we assert the refusal.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::{Tool, ToolRegistry};

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
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// A host tool named `edit` that records if it ever runs — the mask must stop it.
struct EchoEdit;
#[async_trait]
impl Tool for EchoEdit {
    fn name(&self) -> &'static str {
        "edit"
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

/// Build a Holly whose scripted LLM calls `edit` once, wired with the runtime
/// executor over the built-in profiles (which include the masked `explore`).
fn spawn_with_edit_call() -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "edit".into(),
                input: "{\"path\":\"x\"}".into(),
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve the `SetAgent { agent: "explore" }` below.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoEdit);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    holly
}

async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() == sid {
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                break;
            }
        }
    }
    out
}

#[tokio::test]
async fn masked_edit_is_refused_and_never_runs() {
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    // Switch to the read-only `explore` profile: `edit` is masked out entirely.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "please edit".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a masked tool is refused outright, never surfaced for approval"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, .. }
                if output.contains("not available") && output.contains("edit")
        )),
        "masked edit should report it is not available to this agent; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "the masked edit tool must never run"
    );
}

#[tokio::test]
async fn build_profile_runs_edit_unmasked() {
    // Control: the default `build` profile has no mask, so `edit` runs normally.
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "edit it".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "unmasked build should run edit; got {events:?}"
    );
}
