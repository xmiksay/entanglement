//! Integration tests for permission dispatch, relocated from core to the
//! runtime tool executor (#59). Core emits a `ToolExec` for every host tool;
//! `spawn_tool_executor` resolves `Allow | Ask | Deny` against the active
//! `AgentProfile` and drives the approval round-trip on `Ask`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmSession, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry,
    SessionId, Tool, ToolCall, ToolRegistry,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;

/// An LLM that replays scripted responses in order, then plain text (so a turn
/// loop that re-prompts after a tool call terminates).
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

/// A trivial host tool named `bash` so it slots into the built-in profiles
/// (build: Allow, plan: Ask, explore: Deny).
struct EchoBash;
#[async_trait]
impl Tool for EchoBash {
    fn name(&self) -> &'static str {
        "bash"
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

/// Build a Holly whose scripted LLM calls `bash` once, plus a registry with the
/// `EchoBash` tool and the runtime tool executor wired to `profiles`.
fn spawn_with_bash_call_using(input: &str, profiles: ProfileRegistry) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: input.into(),
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(ScriptedLlm::new((*scripted).clone())))
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let _executor = spawn_tool_executor(&holly, reg, profiles);
    holly
}

/// Wire the built-in profiles (build/plan/explore).
fn spawn_with_bash_call(input: &str) -> Holly {
    spawn_with_bash_call_using(input, ProfileRegistry::new())
}

/// Collect events for `sid` until `Done`, with a safety timeout.
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
async fn allow_runs_without_approval() {
    // build profile (default Allow): bash runs directly, no ToolRequest.
    let holly = spawn_with_bash_call("echo hi");
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "Allow must not ask for approval"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: echo hi")),
        "Allow should run the tool; got {events:?}"
    );
}

#[tokio::test]
async fn deny_refuses_without_request() {
    // A profile that *advertises* bash (no tool mask) but denies it via the
    // permission profile — so this exercises the `Deny` dispatch path, distinct
    // from the physical tool mask (#116), which would refuse bash before
    // permission even resolves (see the `tool_mask` integration test).
    let mut profiles = ProfileRegistry::new();
    profiles.insert(AgentProfile {
        name: "denybash".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        permission: PermissionProfile::new(Permission::Deny),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    });
    let holly = spawn_with_bash_call_using("rm -rf", profiles);
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "denybash".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "rm".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "no approval expected on deny"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "Deny should report a denial; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "Deny must not run the tool"
    );
}

#[tokio::test]
async fn ask_emits_request_then_runs_on_approve() {
    // plan profile: bash → Ask. Approve after the request; the tool then runs.
    let holly = spawn_with_bash_call("ls");
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "run".into(),
        })
        .await
        .unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(got_request, "expected a ToolRequest under plan profile");

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: ls")));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn ask_rejected_reports_rejection() {
    // plan profile: bash → Ask. Reject the request; the tool never runs.
    let holly = spawn_with_bash_call("ls");
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "run".into(),
        })
        .await
        .unwrap();

    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { .. }) {
            break;
        }
    }
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id: "t1".into(),
            reason: Some("nope".into()),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("rejected") && output.contains("nope"))
        ),
        "reject should surface a rejection with the reason; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "reject must not run the tool"
    );
}
