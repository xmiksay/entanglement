//! Integration test for the runtime-owned `update_tasks` state tool (#231,
//! ADR-0049). `propose_plan` — the sole plan-authorship tool since #513,
//! ADR-0145 — has its own coverage in `tests/propose_plan.rs`.
//!
//! `update_tasks` round-trips via `ToolExec`/`ToolResult` like every host tool:
//! the runtime executor resolves the ordinary `Allow`/`Ask`/`Deny` permission
//! from the session's mode (ADR-0207 stage 4), emits the `TaskList` snapshot
//! on success, and acks the model. A read-only mode cannot mutate task state
//! (#175) — refused (or gated behind an approval) before any snapshot is
//! emitted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, ProfileRegistry, SessionId, ToolCall,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver, SandboxConfig,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_runner::{spawn_tool_executor, spawn_tool_executor_with_policy};
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
            provider_meta: None,
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

/// Like [`spawn_calling`], but graded from a caller-supplied `mode_table`
/// instead of `ModeTable::builtin()` — for a scenario none of the four
/// built-in modes covers (ADR-0207 stage 4 grades from the session's mode,
/// not its `AgentProfile`).
fn spawn_calling_with_mode_table(
    tool: &str,
    input: &str,
    profiles: ProfileRegistry,
    mode_table: Arc<ModeTable>,
) -> Holly {
    let scripted = Arc::new(vec![LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: tool.into(),
            input: input.into(),
            provider_meta: None,
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
    let reg = ToolRegistry::new();
    let shared_tools = reg.shared();
    let active = Arc::new(Mutex::new(HashMap::new()));
    let perm_modes = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        perm_modes.clone(),
        mode_table,
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        active,
        perm_modes,
        resolver,
        grants,
        Default::default(),
        None,
        SandboxConfig::none(),
        Arc::new(PlanFileRegistry::new()),
        None,
        None,
        None,
    );
    holly
}

/// A single-mode table named `"build"` (matching `DEFAULT_MODE`, so no
/// `SetMode` is needed) that denies `update_tasks` by its literal name —
/// the mode-based analog of `perm_profile`'s old bare `update_tasks: deny`
/// rule.
fn deny_update_tasks_mode_table() -> Arc<ModeTable> {
    let mode = Mode {
        name: "build".to_string(),
        default: Permission::Allow,
        rules: Rules::from_lists(&["update_tasks".to_string()], &[], &[]),
        limits: Limits::default(),
        sandbox: None,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
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
        if ev.session() != Some(sid) {
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

#[tokio::test]
async fn update_tasks_allow_emits_tasklist_and_acks() {
    // `update_tasks` carries `Capability::Control` (ADR-0207 §3) but has no
    // dedicated `Intercept` route, so it grades through `build` mode's
    // `default: prompt` (ADR-0207 §4 — a deliberate change from the old
    // `build` agent's `default: allow`, since no built-in mode writes an
    // explicit `control` rule): approving the parked request is what runs it,
    // after which the runtime emits the `TaskList` snapshot + a "tasks
    // updated" ack.
    let holly = spawn_calling(
        "update_tasks",
        r#"{"content":"- [x] a\n- [ ] b"}"#,
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "update_tasks") {
            break;
        }
    }
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "c1".into(),
            scope: entanglement_core::ApprovalScope::Once,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        let done = matches!(ev, OutEvent::Done { .. });
        events.push(ev);
        if done {
            break;
        }
    }

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
async fn read_only_research_mode_gates_update_tasks_behind_approval() {
    // #175, ADR-0207: the tool mask `explore`'s allowlist used to enforce is
    // retired. `update_tasks` carries `Capability::Control` (ADR-0207 §3),
    // but has no dedicated `Intercept` route of its own, so it still falls
    // through to the generic mode-graded dispatch path; none of the
    // built-in modes write an explicit `control` rule, so it resolves to
    // `research`'s coarse `default: prompt`. Rejecting the resulting
    // approval is what proves the read-only mode never mutates tasks
    // unasked — the security property this test used to prove via a masked-
    // then-denied round-trip.
    let holly = spawn_calling(
        "update_tasks",
        r#"{"content":"- [ ] sneaky"}"#,
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
    );
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "research".into(),
        })
        .await
        .unwrap();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::ToolRequest { tool, .. } = &ev {
            if tool == "update_tasks" {
                got_request = true;
                break;
            }
        }
    }
    assert!(
        got_request,
        "update_tasks under research mode's default must park an approval, not run unasked"
    );

    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id: "c1".into(),
            reason: None,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        let done = matches!(ev, OutEvent::Done { .. });
        events.push(ev);
        if done {
            break;
        }
    }

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::TaskList { .. })),
        "a rejected update_tasks must never emit a TaskList; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, .. }
                if tool == "update_tasks" && output.contains("rejected")
        )),
        "the rejection must surface as the tool's result; got {events:?}"
    );
}

#[tokio::test]
async fn permission_deny_closes_task_mutation() {
    // The ordinary permission path also gates it: a mode that *denies*
    // `update_tasks` by name refuses the call — the #175 fix as a mode rule
    // (ADR-0207 stage 4 grades from the session's mode, not its
    // `AgentProfile`).
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = spawn_calling_with_mode_table(
        "update_tasks",
        r#"{"content":"- [ ] x"}"#,
        profiles,
        deny_update_tasks_mode_table(),
    );
    let sid = SessionId::new("s1");
    let events = collect_until_done(&holly, &sid, None).await;

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
                if tool == "update_tasks" && output.contains("denied by mode")
        )),
        "denied update_tasks must surface a mode refusal; got {events:?}"
    );
}
