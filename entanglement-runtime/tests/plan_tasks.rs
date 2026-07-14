//! Integration test for the runtime-owned `update_plan`/`update_tasks` state
//! tools (#231, ADR-0049).
//!
//! They round-trip via `ToolExec`/`ToolResult` like every host tool: the runtime
//! executor resolves the ordinary `Allow`/`Ask`/`Deny` permission (plus the #116
//! tool mask), emits the `Plan`/`TaskList` snapshot on success, and acks the
//! model. A read-only profile cannot mutate task state (#175) — refused by the
//! mask or by permission before any snapshot is emitted.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry, SessionId,
    ToolCall,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::ToolRegistry;

/// Replays one scripted response, then plain text so the turn terminates.
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

/// A Holly whose scripted LLM calls `tool(input)` once, wired to a runtime tool
/// executor over the given profile registry (empty host registry — state tools
/// never touch it).
fn spawn_calling(tool: &str, input: &str, profiles: ProfileRegistry) -> Holly {
    let scripted = Arc::new(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: tool.into(),
            input: input.into(),
        }],
    }]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(
        &holly,
        ToolRegistry::new(),
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    holly
}

/// Drive a prompt (optionally switching agent first) and collect the session's
/// events until `Done`.
async fn collect_until_done(holly: &Holly, sid: &SessionId, agent: Option<&str>) -> Vec<OutEvent> {
    let mut sub = holly.subscribe();
    if let Some(a) = agent {
        holly
            .send(InMsg::SetAgent {
                session: sid.clone(),
                agent: a.into(),
            })
            .await
            .unwrap();
    }
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != sid {
            continue;
        }
        let done = matches!(ev, OutEvent::Done { .. });
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

/// A profile with an inherit-all tool mask and the given permission.
fn perm_profile(name: &str, permission: PermissionProfile) -> AgentProfile {
    AgentProfile {
        name: name.into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        permission,
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
    }
}

#[tokio::test]
async fn update_tasks_allow_emits_tasklist_and_acks() {
    // The default `build` profile is Allow-all: `update_tasks` runs and the
    // runtime emits the `TaskList` snapshot + a "tasks updated" ack.
    let holly = spawn_calling(
        "update_tasks",
        r#"{"content":"- [x] a\n- [ ] b"}"#,
        entanglement_runtime::agents::built_in_registry(),
    );
    let sid = SessionId::new("s1");
    let events = collect_until_done(&holly, &sid, None).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::TaskList { content, .. } if content == "- [x] a\n- [ ] b"
        )),
        "update_tasks must emit a TaskList snapshot; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == "update_tasks" && output == "tasks updated"
        )),
        "update_tasks must fold a 'tasks updated' ack; got {events:?}"
    );
}

#[tokio::test]
async fn update_plan_allow_emits_plan_snapshot() {
    let mut reg = entanglement_runtime::agents::built_in_registry();
    reg.insert(perm_profile(
        "author",
        PermissionProfile::new(Permission::Allow),
    ));
    let holly = spawn_calling("update_plan", r##"{"content":"# Plan\n1. do"}"##, reg);
    let sid = SessionId::new("s1");
    let events = collect_until_done(&holly, &sid, Some("author")).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::Plan { content, .. } if content == "# Plan\n1. do"
        )),
        "update_plan must emit a Plan snapshot; got {events:?}"
    );
}

#[tokio::test]
async fn read_only_explore_cannot_mutate_tasks_via_mask() {
    // #175: the read-only `explore` profile's allowlist omits `update_tasks`, so
    // the mask refuses a (hallucinated) call before it can mutate task state — no
    // `TaskList` snapshot, and the model is told the tool is unavailable.
    let holly = spawn_calling(
        "update_tasks",
        r#"{"content":"- [ ] sneaky"}"#,
        entanglement_runtime::agents::built_in_registry(),
    );
    let sid = SessionId::new("s1");
    let events = collect_until_done(&holly, &sid, Some("explore")).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::TaskList { .. })),
        "a read-only agent must not emit a TaskList; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == "update_tasks" && output.contains("not available")
        )),
        "masked update_tasks must be refused; got {events:?}"
    );
}

#[tokio::test]
async fn permission_deny_closes_task_mutation() {
    // The ordinary permission path also gates it: an inherit-all profile that
    // *denies* `update_tasks` refuses the call (no mask involved) — the #175 fix
    // as a permission-profile entry.
    let mut reg = entanglement_runtime::agents::built_in_registry();
    reg.insert(perm_profile(
        "locked",
        PermissionProfile::new(Permission::Allow).with("update_tasks", Permission::Deny),
    ));
    let holly = spawn_calling("update_tasks", r#"{"content":"- [ ] x"}"#, reg);
    let sid = SessionId::new("s1");
    let events = collect_until_done(&holly, &sid, Some("locked")).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::TaskList { .. })),
        "denied update_tasks must not emit a TaskList; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == "update_tasks" && output.contains("denied by permission")
        )),
        "denied update_tasks must surface a permission refusal; got {events:?}"
    );
}
