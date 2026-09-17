//! Integration test for the runtime-owned `update_tasks` state tool (#231,
//! ADR-0049). `propose_plan` — the sole plan-authorship tool since #513,
//! ADR-0145 — has its own coverage in `tests/propose_plan.rs`.
//!
//! `update_tasks` carries no host resource (#231) and declares
//! `Capability::Control` (ADR-0207 §3): unlike an ordinary host tool it is
//! never graded, so it runs and emits its `TaskList` snapshot + ack
//! unconditionally, under every mode and regardless of an explicit rule
//! naming it (gap 1 of ADR-0207 stage 4b — it used to fall through to the
//! generic `Allow`/`Ask`/`Deny` ladder like a normal tool, which the #175
//! read-only-mutation concern originally relied on; that concern is now
//! closed structurally instead, since a display-only task outline is not a
//! host mutation).

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
async fn update_tasks_runs_and_acks_unconditionally() {
    // `update_tasks` carries `Capability::Control` (ADR-0207 §3, gap 1 of
    // stage 4b): it is never graded, so it runs the moment the model calls
    // it — no approval round-trip, `build` mode's own `default: prompt`
    // never applies to it — after which the runtime emits the `TaskList`
    // snapshot + a "tasks updated" ack.
    let holly = spawn_calling(
        "update_tasks",
        r#"{"content":"- [x] a\n- [ ] b"}"#,
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
    );
    let sid = SessionId::new("s1");
    let events = collect_until_done(&holly, &sid, None).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a Control tool must never park an approval; got {events:?}"
    );
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
async fn read_only_research_mode_does_not_gate_update_tasks() {
    // #175's old concern — a read-only mode must not let the model mutate
    // task state unasked — is superseded, not violated, by ADR-0207 §3:
    // `update_tasks` is `Capability::Control` (session bookkeeping, not a
    // host mutation) and so is never graded, running the same way under
    // `research` as under any other mode. This is the flip side of
    // `permission_deny_closes_task_mutation` below: neither a mode's
    // `default` nor an explicit rule naming it reaches a Control tool.
    let holly = spawn_calling(
        "update_tasks",
        r#"{"content":"- [ ] sneaky"}"#,
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "research".into(),
        })
        .await
        .unwrap();
    let events = collect_until_done(&holly, &sid, None).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "research mode must not gate a Control tool behind approval; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::TaskList { content, .. } if content == "- [ ] sneaky"
        )),
        "update_tasks must run and emit a TaskList under research mode too; got {events:?}"
    );
}

#[tokio::test]
async fn permission_deny_closes_task_mutation() {
    // A mode rule naming `update_tasks` by name is inert — `Capability::Control`
    // is never graded (ADR-0207 §3, gap 1 of stage 4b), so the #175 concern
    // this test used to close as a mode-rule deny is now closed structurally
    // instead: there is no host mutation here to gate at all, only the
    // session's own display task outline.
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
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a Control tool must never park an approval either; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::TaskList { content, .. } if content == "- [ ] x"
        )),
        "a mode rule naming update_tasks must not deny it; got {events:?}"
    );
}
