//! An alias must not launder permissions (#560 P8, plan item §3): a
//! skill-declared alias grades and executes **exactly as if the model had
//! called the tool it wraps directly** — never through its own namespaced
//! name. Proven end-to-end through the real tool executor (not a unit test
//! on the rewrite function alone), mirroring `permission_dispatch.rs`'s
//! harness: a profile that denies `bash` outright but leaves every other
//! (unlisted) tool name at its permissive `default: allow` would — if the
//! alias's own name were graded instead of `bash`'s — let a
//! `skill__x__alias_bash` call straight through. It must not.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry, SessionId,
    ToolCall,
};
use entanglement_runtime::skills::tools::{register_skill_tools, SkillToolDef};
use entanglement_runtime::skills::{SkillMeta, SkillRegistry};
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

/// One skill ("x") declaring a single alias tool wrapping `bash`.
fn skill_with_bash_alias() -> SkillRegistry {
    let mut skills = SkillRegistry::default();
    skills.insert(SkillMeta {
        name: "x".to_string(),
        description: "d".to_string(),
        user_only: false,
        allowed_tools: None,
        root_dir: None,
        body: String::new(),
        tools: vec![SkillToolDef::Alias {
            name: "alias_bash".to_string(),
            target: "bash".to_string(),
            args: serde_json::Map::new(),
            description: String::new(),
        }],
    });
    skills
}

/// A profile denying `bash` outright, everything else at the permissive
/// `default: allow` — the shape that would let a mis-graded alias slip
/// through if it graded under its own name instead of `bash`'s.
fn denies_bash_but_defaults_allow() -> ProfileRegistry {
    let mut profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    profiles.insert(AgentProfile {
        name: "denybash".into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Allow).with("bash", Permission::Deny),
        tools: None,
        disallowed_tools: Vec::new(),
        can_spawn: None,
        spawnable_agents: None,
        sandbox: None,
    });
    profiles
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

#[tokio::test]
async fn alias_grades_as_its_underlying_tool_not_its_own_name() {
    let skills = skill_with_bash_alias();
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    register_skill_tools(
        &mut reg,
        &skills,
        &entanglement_core::HttpClient::new().unwrap(),
    );
    assert!(
        reg.contains("skill__x__alias_bash"),
        "alias must have registered"
    );

    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "skill__x__alias_bash".into(),
                input: "{}".into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let profiles = denies_bash_but_defaults_allow();
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
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );

    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "denybash".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    // Never ran (that would prove the alias slipped through ungraded).
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "the aliased bash call must never execute under a profile that denies bash: {events:?}"
    );
    // Denied — and attributed to `bash` (the rewritten name), not the
    // alias's own namespaced name, proving the grading decision itself (not
    // just the outcome) operated on the underlying tool.
    let denial = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                output, is_error, ..
            } if *is_error => Some(output.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a denial output; got {events:?}"));
    assert!(
        denial.contains("tool `bash` denied"),
        "denial must name the underlying tool, not the alias: {denial}"
    );
    assert!(
        !denial.contains("skill__x__alias_bash"),
        "denial must not leak the alias's own namespaced name: {denial}"
    );
}
