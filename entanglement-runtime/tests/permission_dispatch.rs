//! Integration tests for permission dispatch, relocated from core to the
//! runtime tool executor (#59). Core emits a `ToolExec` for every host tool;
//! `spawn_tool_executor` resolves `Allow | Ask | Deny` against the active
//! `AgentProfile` and drives the approval round-trip on `Ask`.

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, ApprovalScope, EngineConfig, Holly, InMsg, Llm,
    LlmRequest, LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry,
    SessionId, ToolCall,
};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver, SandboxConfig,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_runner::{spawn_tool_executor, spawn_tool_executor_with_policy};
use entanglement_runtime::{Tool, ToolRegistry};

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
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
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
                provider_meta: None,
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
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    holly
}

/// Wire the built-in profiles (build/plan/explore).
fn spawn_with_bash_call(input: &str) -> Holly {
    spawn_with_bash_call_using(input, entanglement_runtime::agents::built_in_registry())
}

/// Built-ins plus an `askbash` profile that *advertises* bash (no tool mask) but
/// grades it `Ask` — the built-in `plan` now physically masks bash out (#140), so
/// the Ask dispatch path needs a profile that still lets bash through the mask.
fn ask_bash_registry() -> ProfileRegistry {
    let mut profiles = entanglement_runtime::agents::built_in_registry();
    profiles.insert(AgentProfile {
        name: "askbash".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Ask),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    });
    profiles
}

/// Collect events for `sid` until `Done`, with a safety timeout.
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

#[tokio::test]
async fn allow_runs_without_approval() {
    // build profile (default Allow): bash runs directly, no ToolRequest.
    let holly = spawn_with_bash_call("echo hi");
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
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
    let mut profiles = entanglement_runtime::agents::built_in_registry();
    profiles.insert(AgentProfile {
        name: "denybash".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Deny),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
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
    holly.send(InMsg::prompt(sid.clone(), "rm")).await.unwrap();
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
    // askbash profile: bash → Ask. Approve after the request; the tool then runs.
    let holly = spawn_with_bash_call_using("ls", ask_bash_registry());
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "askbash".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(got_request, "expected a ToolRequest under askbash profile");

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: Default::default(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: ls")));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

/// A profile that grades `bash` by its command (#173): `git *` runs outright,
/// `rm *` is denied, anything else asks.
fn scoped_bash_registry() -> ProfileRegistry {
    let mut profiles = entanglement_runtime::agents::built_in_registry();
    profiles.insert(AgentProfile {
        name: "scopedbash".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Ask)
            .with("bash(git *)", Permission::Allow)
            .with("bash(rm *)", Permission::Deny),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    });
    profiles
}

#[tokio::test]
async fn argument_scoped_allow_runs_matching_command_without_approval() {
    // `git status` matches `bash(git *): allow` → runs directly, no ToolRequest.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "git status" }).to_string(),
        scoped_bash_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedbash".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an argument-scoped Allow must not ask for approval; got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("git status"))
        ),
        "the matching command should run; got {events:?}"
    );
}

#[tokio::test]
async fn argument_scoped_deny_blocks_matching_command() {
    // `rm -rf /` matches `bash(rm *): deny` → refused, never runs.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "rm -rf /" }).to_string(),
        scoped_bash_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedbash".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "rm")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "the matching command should be denied; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "a denied command must not run"
    );
}

#[tokio::test]
async fn argument_scoped_falls_through_to_coarse_ask() {
    // `ls` matches neither refined rule → the coarse `bash: ask` grade applies.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls -la" }).to_string(),
        scoped_bash_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedbash".into(),
        })
        .await
        .unwrap();
    let mut watch = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "list"))
        .await
        .unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "a command matching no refined rule should fall through to the coarse Ask"
    );
}

/// A profile that grades `bash` by its `workdir` (#425): `/tmp/*` runs
/// outright, `/etc/*` is denied, anything else asks.
fn scoped_workdir_bash_registry() -> ProfileRegistry {
    let mut profiles = entanglement_runtime::agents::built_in_registry();
    profiles.insert(AgentProfile {
        name: "scopedworkdir".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Ask)
            .with("bash{/tmp/*}", Permission::Allow)
            .with("bash{/etc/*}", Permission::Deny),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    });
    profiles
}

#[tokio::test]
async fn workdir_scoped_allow_runs_matching_workdir_without_approval() {
    // A `bash` call under `/tmp` matches `bash{/tmp/*}: allow` → runs directly.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls", "workdir": "/tmp/scratch" }).to_string(),
        scoped_workdir_bash_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedworkdir".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a workdir-scoped Allow must not ask for approval; got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("/tmp/scratch"))
        ),
        "the matching call should run; got {events:?}"
    );
}

#[tokio::test]
async fn workdir_scoped_deny_blocks_matching_workdir() {
    // A `bash` call under `/etc` matches `bash{/etc/*}: deny` → refused.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls", "workdir": "/etc/cron.d" }).to_string(),
        scoped_workdir_bash_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedworkdir".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "the matching call should be denied; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "a denied call must not run"
    );
}

#[tokio::test]
async fn workdir_scoped_falls_through_to_coarse_ask_outside_every_pattern() {
    // A `bash` call under neither scoped workdir falls to the coarse `ask`.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls", "workdir": "/home/x" }).to_string(),
        scoped_workdir_bash_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedworkdir".into(),
        })
        .await
        .unwrap();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "a workdir matching no scoped rule should fall through to the coarse Ask"
    );
}

#[tokio::test]
async fn ask_rejected_reports_rejection() {
    // askbash profile: bash → Ask. Reject the request; the tool never runs.
    let holly = spawn_with_bash_call_using("ls", ask_bash_registry());
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "askbash".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();

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

/// Spawn a Holly whose scripted LLM calls `bash` once per turn (ids `t1`, `t2`)
/// with the given `command`, so two prompts drive two identical calls. Wired to
/// the `askbash` profile (bash → Ask) so both calls would prompt absent a grant.
fn spawn_two_ask_bash_calls(command: &str) -> Holly {
    let call = |id: &str| LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": command }).to_string(),
            provider_meta: None,
        }],
    };
    let ok = || LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call("t1"), ok(), call("t2"), ok()]);
    let profiles = ask_bash_registry();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );
    holly
}

/// A hallucinated tool name must never reach the `Ask` approval ladder (#437):
/// the registry miss is now discovered *before* permission resolution, so an
/// unknown-tool call gets an immediate `ToolOutput` — no `ToolRequest`, no wait
/// for a human — even under a profile that would otherwise ask for `bash`.
#[tokio::test]
async fn unknown_tool_is_rejected_before_the_permission_ladder() {
    let call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "bsah".into(),
            input: "{}".into(),
            provider_meta: None,
        }],
    };
    let ok = LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call, ok]);
    let profiles = ask_bash_registry();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );

    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "askbash".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an unknown tool must never reach the Ask approval prompt; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. }
            if output.contains("unknown tool") && output.contains("did you mean `bash`"))),
        "expected an immediate unknown-tool reply with a closest-match hint; got {events:?}"
    );
}

/// An `Approve { scope: Session }` (#174) records an in-memory grant, so the next
/// *identical* call in the same session runs without a second `ToolRequest`.
#[tokio::test]
async fn session_grant_skips_the_second_prompt() {
    let holly = spawn_two_ask_bash_calls("ls");
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "askbash".into(),
        })
        .await
        .unwrap();

    // Turn 1: the Ask prompts; approve it for the session.
    let sub1 = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            asked = true;
            break;
        }
    }
    assert!(asked, "turn 1 should prompt for approval");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: entanglement_core::ApprovalScope::Session,
        })
        .await
        .unwrap();
    let turn1 = collect(sub1, &sid).await;
    assert!(
        turn1
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("ls"))),
        "turn 1 should run the approved command; got {turn1:?}"
    );

    // Turn 2: the identical call must NOT prompt again — the session grant runs it.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "run again"))
        .await
        .unwrap();
    let turn2 = collect(sub2, &sid).await;
    assert!(
        !turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a session-granted call must not ask again; got {turn2:?}"
    );
    assert!(
        turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("ls"))),
        "turn 2 should still run the command; got {turn2:?}"
    );
}

// --- #485, ADR-0125: absolute-inside-root path args grade like the relative
// spelling ------------------------------------------------------------------

/// A trivial `read` host tool, standing in for the real filesystem tool — this
/// module only exercises permission grading, never actual file I/O.
struct EchoRead;
#[async_trait]
impl Tool for EchoRead {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("read")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

/// A profile that grades `read` by an arg-scoped rule authored root-relative
/// (#173): `src/*` runs outright, anything else asks.
fn scoped_read_registry() -> ProfileRegistry {
    let mut profiles = entanglement_runtime::agents::built_in_registry();
    profiles.insert(AgentProfile {
        name: "scopedread".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Ask).with("read(src/*)", Permission::Allow),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    });
    profiles
}

/// Build a Holly whose scripted LLM calls `read` twice — `input1` (id `t1`)
/// then `input2` (id `t2`) — wired to `profiles` through a [`ProfileResolver`]
/// with `root` set (#485, ADR-0125), mirroring `main.rs`'s production wiring
/// rather than the no-root `spawn_tool_executor` convenience wrapper the rest
/// of this file uses.
fn spawn_two_read_calls_rooted(
    root: &Path,
    input1: &str,
    input2: &str,
    profiles: ProfileRegistry,
) -> Holly {
    let call = |id: &str, input: &str| LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "read".into(),
            input: input.into(),
            provider_meta: None,
        }],
    };
    let ok = || LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call("t1", input1), ok(), call("t2", input2), ok()]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoRead);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        active.clone(),
        PermissionProfile::new(Permission::Allow),
        Some(root.to_path_buf()),
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    // Wire the same `root` into the executor's escape-root policy too (#485,
    // ADR-0125) — `dispatch` derives its grading-arg root from this param, so
    // it must match the `ProfileResolver`'s, mirroring `main.rs`'s production
    // wiring where both come from the same canonicalized `root`.
    let escape_root = entanglement_runtime::tool_runner::EscapeRoot {
        root: root.to_path_buf(),
        store: Arc::new(entanglement_runtime::extra_roots::ExtraRootStore::ephemeral()),
    };
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        reg.shared(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        active,
        resolver,
        grants,
        Default::default(),
        Some(escape_root),
        SandboxConfig::none(),
    );
    holly
}

/// (a) An absolute path resolving inside root must match the same
/// root-relative arg-scoped rule its relative spelling matches — the bug: the
/// verbatim `/root/src/main.rs` used to fall through `read(src/*)` to the
/// coarse `ask` default, prompting for a call the relative spelling would run
/// outright.
#[tokio::test]
async fn absolute_inside_root_read_matches_the_relative_scoped_rule() {
    let root = Path::new("/home/user/project");
    let holly = spawn_two_read_calls_rooted(
        root,
        &serde_json::json!({ "path": "/home/user/project/src/main.rs" }).to_string(),
        &serde_json::json!({ "path": "/home/user/project/src/main.rs" }).to_string(),
        scoped_read_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedread".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an absolute in-root path matching a root-relative rule must not ask; got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("main.rs"))
        ),
        "the matching call should run; got {events:?}"
    );
}

/// (b) Grant-key stability: a Session grant recorded against the relative
/// spelling of a call must also cover the absolute spelling of the identical
/// file — the bug: the two spellings keyed different grants, so the second
/// (absolute) call still prompted.
#[tokio::test]
async fn session_grant_on_relative_spelling_covers_the_absolute_spelling() {
    let root = Path::new("/home/user/project");
    // `askbash` (this file's default-Ask, no-mask profile, reused here for
    // `read`) — coarse `ask` with no arg-scoped rule, so both calls would
    // prompt absent a grant, isolating this test from the arg-scoped-rule
    // behavior covered above.
    let holly = spawn_two_read_calls_rooted(
        root,
        &serde_json::json!({ "path": "src/main.rs" }).to_string(),
        &serde_json::json!({ "path": "/home/user/project/src/main.rs" }).to_string(),
        ask_bash_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "askbash".into(),
        })
        .await
        .unwrap();

    // Turn 1: the relative spelling prompts; approve it for the session.
    let sub1 = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "read") {
            asked = true;
            break;
        }
    }
    assert!(asked, "turn 1 (relative spelling) should prompt");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Session,
        })
        .await
        .unwrap();
    let turn1 = collect(sub1, &sid).await;
    assert!(
        turn1.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("main.rs"))
        ),
        "turn 1 should run the approved read; got {turn1:?}"
    );

    // Turn 2: the absolute spelling of the SAME file must NOT prompt again.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "run again"))
        .await
        .unwrap();
    let turn2 = collect(sub2, &sid).await;
    assert!(
        !turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "the absolute spelling of an already-granted file must not ask again; got {turn2:?}"
    );
    assert!(
        turn2.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("main.rs"))
        ),
        "turn 2 should still run the read; got {turn2:?}"
    );
}

/// (c) An absolute path resolving OUTSIDE root must stay verbatim and
/// therefore keep asking — a root-relative rule matching an outside path
/// would be a privilege escalation, not a convenience.
#[tokio::test]
async fn absolute_outside_root_still_prompts() {
    let root = Path::new("/home/user/project");
    let holly = spawn_two_read_calls_rooted(
        root,
        &serde_json::json!({ "path": "/etc/passwd" }).to_string(),
        &serde_json::json!({ "path": "/etc/passwd" }).to_string(),
        scoped_read_registry(),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "scopedread".into(),
        })
        .await
        .unwrap();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "read") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "an out-of-root absolute path must not silently match a root-relative rule"
    );
}

// --- #486, ADR-0126: ApprovalScope::SessionDir --------------------------

/// A trivial host tool that just echoes its input, parameterized by name —
/// standing in for `read`/`grep`/`glob`/`edit` in the `SessionDir` tests
/// below; this module only exercises permission grading, never real file I/O.
struct EchoNamed(&'static str);
#[async_trait]
impl Tool for EchoNamed {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran {}: {input}", self.0))
    }
}

/// Build a Holly wired exactly like [`spawn_two_read_calls_rooted`] (a
/// `root`-aware `ProfileResolver` + escape-root policy + `DefaultGrantStore`),
/// but scripted with an arbitrary `(id, tool, input)` call sequence, each
/// followed by a tool-less "ok" turn. Registers `read`/`grep`/`glob`/`edit`
/// `EchoNamed` tools — every tool the `SessionDir` tests below exercise. The
/// scripted queue is a single shared template: each *session* gets its own
/// fresh clone from `llm_factory` (starting at element 0), so a second,
/// independent session that sends exactly one prompt naturally replays this
/// list's *first* call — the deliberate mechanism the cross-session test
/// below relies on to re-ask the identical call a first session had granted.
fn spawn_scripted_calls_rooted(
    root: &Path,
    calls: &[(&str, &str, String)],
    profiles: ProfileRegistry,
) -> Holly {
    let mut responses = Vec::new();
    for (id, tool, input) in calls {
        responses.push(LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: (*id).into(),
                name: (*tool).into(),
                input: input.clone(),
                provider_meta: None,
            }],
        });
        responses.push(LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        });
    }
    let scripted = Arc::new(responses);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoNamed("read"));
    reg.register(EchoNamed("grep"));
    reg.register(EchoNamed("glob"));
    reg.register(EchoNamed("edit"));
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        active.clone(),
        PermissionProfile::new(Permission::Allow),
        Some(root.to_path_buf()),
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let escape_root = entanglement_runtime::tool_runner::EscapeRoot {
        root: root.to_path_buf(),
        store: Arc::new(entanglement_runtime::extra_roots::ExtraRootStore::ephemeral()),
    };
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        reg.shared(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        active,
        resolver,
        grants,
        Default::default(),
        Some(escape_root),
        SandboxConfig::none(),
    );
    holly
}

/// Approving a `read` call with `Approve { scope: SessionDir }` widens the
/// grant to every later `read`/`grep`/`glob` call whose path falls under the
/// approved call's directory — a sibling file, a nested subdirectory, and a
/// directory-rooted `grep`/`glob` all skip the prompt — but `edit` (not in the
/// read-only triad) still asks, and a second session never inherits the grant.
#[tokio::test]
async fn session_dir_grant_widens_the_read_only_triad_but_not_edit_or_other_sessions() {
    let root = Path::new("/home/user/project");
    let calls: Vec<(&str, &str, String)> = vec![
        (
            "t1",
            "read",
            serde_json::json!({ "path": "src/a.rs" }).to_string(),
        ),
        (
            "t2",
            "read",
            serde_json::json!({ "path": "src/b/c.rs" }).to_string(),
        ),
        (
            "t3",
            "grep",
            serde_json::json!({ "path": "src" }).to_string(),
        ),
        (
            "t4",
            "edit",
            serde_json::json!({ "path": "src/a.rs" }).to_string(),
        ),
    ];
    let holly = spawn_scripted_calls_rooted(root, &calls, ask_bash_registry());
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "askbash".into(),
        })
        .await
        .unwrap();

    // Turn 1: `read src/a.rs` prompts; approve it with SessionDir scope.
    let sub1 = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "read") {
            asked = true;
            break;
        }
    }
    assert!(asked, "turn 1 (read src/a.rs) should prompt");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::SessionDir,
        })
        .await
        .unwrap();
    let turn1 = collect(sub1, &sid).await;
    assert!(
        turn1
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("a.rs"))),
        "turn 1 should run the approved read; got {turn1:?}"
    );

    // Turn 2: `read src/b/c.rs` — a nested subdirectory under "src" — must not
    // prompt again.
    let sub2 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn2 = collect(sub2, &sid).await;
    assert!(
        !turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a read under the granted directory must not ask again; got {turn2:?}"
    );

    // Turn 3: `grep {path: "src"}` — the directory itself — must not prompt.
    let sub3 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn3 = collect(sub3, &sid).await;
    assert!(
        !turn3
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a grep rooted at the granted directory must not ask again; got {turn3:?}"
    );

    // Turn 4: `edit src/a.rs` is NOT in the read-only triad — still prompts.
    let mut watch4 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut edit_asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch4.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "edit") {
            edit_asked = true;
            break;
        }
    }
    assert!(
        edit_asked,
        "a SessionDir grant must never widen a mutation tool like edit"
    );

    // A second, independent session replays the same first call (`read
    // src/a.rs`, per `spawn_scripted_calls_rooted`'s doc) and must still be
    // asked — a directory grant never crosses sessions.
    let sid2 = SessionId::new("s2");
    holly
        .send(InMsg::SetAgent {
            session: sid2.clone(),
            agent: "askbash".into(),
        })
        .await
        .unwrap();
    let mut watch2 = holly.subscribe();
    holly.send(InMsg::prompt(sid2.clone(), "go")).await.unwrap();
    let mut other_session_asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch2.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { session, tool, .. } if session == &sid2 && tool == "read")
        {
            other_session_asked = true;
            break;
        }
    }
    assert!(
        other_session_asked,
        "a SessionDir grant must never be inherited by a different session"
    );
}
