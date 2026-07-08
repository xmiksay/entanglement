//! Integration tests for the Holly engine actor: session multiplexing,
//! permission dispatch (allow/ask/deny), and the built-in plan/tasks tools.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmSession, LlmStream, OutEvent, Permission, PermissionProfile, SessionId,
    TaskItem, TaskStatus, ToolCall, ToolRegistry,
};

mod common;
use common::spawn_tool_executor;

/// Collect events for `sid` until `Done`, with a safety timeout.
async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let Ok(recv) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == sid => {
                let done = matches!(ev, OutEvent::Done { .. });
                out.push(ev);
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// An LLM that replays a scripted list of responses, in order.
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<LlmResponse>) -> Self {
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

fn factory(responses: Vec<LlmResponse>) -> EngineConfig {
    // Reverse because ScriptedLlm::complete pops from the back.
    let mut r = responses;
    r.reverse();
    let llm = Arc::new(ScriptedLlm::new(r));
    EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(ScriptedLlm::new(
                llm.responses.lock().unwrap().clone(),
            )))
        }),
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn dummy_turn_streams_text_and_done() {
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "hello".into(),
        tool_calls: vec![],
    }]));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "hello")));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn allow_permission_runs_without_approval() {
    // build profile (default Allow): tool runs directly, no ToolRequest.
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            input: "echo hi".into(),
        }],
    }]));
    // Allow runs without approval, but execution is now a runtime round-trip
    // (#58): an empty registry reports "unknown tool", as inline exec once did.
    spawn_tool_executor(&holly, ToolRegistry::new());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "run it".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "no approval expected"
    );
    assert!(events.iter().any(
        |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("unknown tool"))
    ));
}

#[tokio::test]
async fn allow_tool_round_trips_through_toolexec() {
    // Core relocated execution (#58): an Allow tool must be emitted as a
    // ToolExec (never a ToolRequest), and the runtime executor's output must
    // surface as ToolOutput. A real tool proves the value round-trips.
    struct EchoTool;
    #[async_trait]
    impl entanglement_core::Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        async fn run(&self, input: &str) -> anyhow::Result<String> {
            Ok(format!("echoed: {input}"))
        }
    }

    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "echo".into(),
            input: "ping".into(),
        }],
    }]));
    let mut reg = ToolRegistry::new();
    reg.register(EchoTool);
    spawn_tool_executor(&holly, reg);
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
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolExec { tool, request_id, .. } if tool == "echo" && request_id == "t1"
        )),
        "Allow tool should be handed to the runtime via ToolExec; got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "Allow must not ask for approval"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "echoed: ping")),
        "executor output should surface as ToolOutput; got {events:?}"
    );
}

#[tokio::test]
async fn ask_permission_emits_request_then_runs_on_approve() {
    // plan profile: bash → Ask. Send Approve after the request.
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            input: "ls".into(),
        }],
    }]));
    // Ask emits ToolRequest for approval; after Approve, execution is a runtime
    // round-trip (#58) the executor answers.
    spawn_tool_executor(&holly, ToolRegistry::new());
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "run".into(),
        })
        .await
        .unwrap();

    // Wait for the ToolRequest, then approve.
    let mut sub2 = holly.subscribe();
    let mut got_request = false;
    while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub2.recv()).await {
        if let Ok(OutEvent::ToolRequest { .. }) = ev {
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
        .any(|e| matches!(e, OutEvent::ToolRequest { tool, .. } if tool == "bash")));
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::ToolOutput { .. })));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn deny_permission_refuses_without_request() {
    // explore profile: bash → Deny (default deny).
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            input: "rm -rf".into(),
        }],
    }]));
    let sid = SessionId::new("s1");
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
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))));
}

#[tokio::test]
async fn builtin_update_plan_emits_plan_snapshot() {
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "update_plan".into(),
            input: "# Plan\nstep 1".into(),
        }],
    }]));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "plan it".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::Plan { content, .. } if content == "# Plan\nstep 1")));
    assert!(events.iter().any(
        |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("plan updated"))
    ));
}

#[tokio::test]
async fn builtin_update_tasks_emits_tasklist_snapshot() {
    let tasks_json = r#"[{"id":"t1","content":"do","status":"in_progress"}]"#;
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "update_tasks".into(),
            input: tasks_json.into(),
        }],
    }]));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "track".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(events.iter().any(|e| matches!(e, OutEvent::TaskList { tasks, .. } if tasks.len() == 1 && tasks[0].status == TaskStatus::InProgress)));
}

#[tokio::test]
async fn harness_set_tasks_and_set_plan_emit_snapshots() {
    let holly = Holly::spawn(factory(vec![LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    }]));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::SetTasks {
            session: sid.clone(),
            tasks: vec![TaskItem {
                id: "t1".into(),
                content: "x".into(),
                status: TaskStatus::Pending,
            }],
        })
        .await
        .unwrap();
    holly
        .send(InMsg::SetPlan {
            session: sid.clone(),
            content: "strategy".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::TaskList { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::Plan { content, .. } if content == "strategy")));
}

#[tokio::test]
async fn set_agent_emits_agent_changed() {
    let holly = Holly::spawn(EngineConfig::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();

    let mut saw_build = false;
    let mut saw_plan = false;
    while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let Ok(OutEvent::AgentChanged { agent, .. }) = &ev {
            if agent == "build" {
                saw_build = true;
            }
            if agent == "plan" {
                saw_plan = true;
            }
        }
        if saw_plan {
            break;
        }
    }
    assert!(saw_build, "session should start under build");
    assert!(saw_plan, "should switch to plan");
}

#[tokio::test]
async fn two_sessions_are_independent() {
    let holly = Holly::spawn(factory(vec![
        LlmResponse {
            text: "from-s1".into(),
            tool_calls: vec![],
        },
        LlmResponse {
            text: "from-s2".into(),
            tool_calls: vec![],
        },
    ]));
    let s1 = SessionId::new("s1");
    let s2 = SessionId::new("s2");
    let sub1 = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: s1.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::Prompt {
            session: s2.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();

    let e1 = collect(sub1, &s1).await;
    assert!(e1
        .iter()
        .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "from-s1")));
    // s1's events should not contain s2's text.
    assert!(!e1
        .iter()
        .any(|e| matches!(e, OutEvent::TextDelta { text, .. } if text == "from-s2")));
}

#[tokio::test]
async fn custom_profile_is_selectable() {
    let mut cfg = EngineConfig::default();
    cfg.profiles.insert(AgentProfile {
        name: "paranoid".into(),
        mode: AgentMode::Primary,
        system_prompt: "Ask before anything.".into(),
        model: None,
        permission: PermissionProfile::new(Permission::Ask),
    });
    let holly = Holly::spawn(cfg);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "paranoid".into(),
        })
        .await
        .unwrap();

    let mut ok = false;
    while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let Ok(OutEvent::AgentChanged { agent, .. }) = ev {
            if agent == "paranoid" {
                ok = true;
                break;
            }
        }
    }
    assert!(ok);
}
