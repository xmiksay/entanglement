//! An alias must not launder permissions (#560 P8, plan item §3): a
//! skill-declared alias grades and executes **exactly as if the model had
//! called the tool it wraps directly** — never through its own namespaced
//! name. Proven end-to-end through the real tool executor (not a unit test
//! on the rewrite function alone), mirroring `permission_dispatch.rs`'s
//! harness: a mode that denies `bash` outright but leaves every other
//! (unlisted) tool name at its permissive `default: allow` would — if the
//! alias's own name were graded instead of `bash`'s — let a
//! `skill__x__alias_bash` call straight through. It must not. Since ADR-0207
//! stage 4, grading comes from the session's permission mode, not its agent
//! profile, so the fixture is a `Mode`, not an `AgentProfile`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver, SandboxConfig,
};
use entanglement_runtime::skills::tools::{register_skill_tools, SkillToolDef};
use entanglement_runtime::skills::{SkillMeta, SkillRegistry};
use entanglement_runtime::tool_runner::spawn_tool_executor_with_policy;
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

/// A mode denying `bash` outright, everything else at the permissive
/// `default: allow` — the shape that would let a mis-graded alias slip
/// through if it graded under its own name instead of `bash`'s. Named
/// `"build"` so a session picks it up as `DEFAULT_MODE` with no `SetMode`.
fn denies_bash_but_defaults_allow() -> Arc<ModeTable> {
    let mode = Mode {
        name: "build".to_string(),
        default: Permission::Allow,
        rules: Rules::from_lists(&["bash".to_string()], &[], &[]),
        limits: Limits::default(),
        sandbox: None,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
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
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let shared_reg = reg.shared();
    let active = Arc::new(Mutex::new(HashMap::new()));
    let perm_modes = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        perm_modes.clone(),
        denies_bash_but_defaults_allow(),
        shared_reg.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        shared_reg,
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

    let sid = SessionId::new("s1");
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
