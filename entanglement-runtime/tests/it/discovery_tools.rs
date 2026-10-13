//! ADR-0226's discovery tools through the real executor: `list_agents`,
//! `list_models`, `list_skills`, `list_mcp` and `mcp_tools` are `Control`
//! reads — answered under a mode that denies every graded tool, never
//! parking an approval — and each renders its roster from live state.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Catalog, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::skills::{SkillMeta, SkillRegistry};
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::ToolRegistry;

struct EchoRead;
#[async_trait]
impl entanglement_runtime::Tool for EchoRead {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("read")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.responses.lock().unwrap().pop().unwrap_or(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
            ..Default::default()
        });
        Ok(stream_from_response(resp))
    }
}

/// `build` with `default: deny` and no rules: any *graded* call is refused.
fn deny_all_table() -> Arc<ModeTable> {
    Arc::new(
        ModeTable::new(vec![Mode {
            name: "build".to_string(),
            default: Permission::Deny,
            rules: Rules::default(),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        }])
        .expect("single-mode table is valid"),
    )
}

const CATALOG: &str = "providers:\n\
  \x20 - name: local\n\
  \x20   default_model: tiny\n\
  \x20   models:\n\
  \x20     - id: tiny\n\
  \x20       display_name: Tiny Model\n";

/// One turn on a fresh session whose model makes every `(name, input)` call
/// in one round; returns the session's events up to `Done`.
async fn run_calls(calls: &[(&str, &str)]) -> Vec<OutEvent> {
    let batch = LlmResponse {
        text: String::new(),
        tool_calls: calls
            .iter()
            .enumerate()
            .map(|(i, (name, input))| ToolCall {
                id: format!("c{i}"),
                name: (*name).into(),
                input: (*input).into(),
                provider_meta: None,
            })
            .collect(),
        ..Default::default()
    };
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let scripted = Arc::new(vec![LlmResponse::default(), batch]);
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: Mutex::new((*scripted).clone()),
            }) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    });
    let mut skills = SkillRegistry::default();
    skills.insert(SkillMeta {
        name: "git".into(),
        description: "commit helpers".into(),
        user_only: false,
        allowed_tools: None,
        root_dir: None,
        body: String::new(),
        lib: None,
    });
    let mut reg = ToolRegistry::new();
    reg.register(EchoRead);
    let tools = reg.shared();
    let base = PermissionProfile::new(Permission::Allow);
    let perm_modes = crate::mode_support::perm_modes();
    let table = deny_all_table();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        table.clone(),
        tools.clone(),
        base.clone(),
        None,
    ));
    let catalog: Catalog = serde_yaml::from_str(CATALOG).expect("catalog yaml");
    let _executor = ToolExecutor::builder(&holly, tools)
        .agents(profiles)
        .live_skills(Arc::new(RwLock::new(Arc::new(skills))))
        .base(base)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(Arc::new(DefaultGrantStore::load_from(None)) as Arc<dyn GrantStore>)
        .mode_table(table)
        .catalog(Arc::new(catalog))
        .spawn();
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        if ev.session() == Some(&sid) {
            let done = matches!(ev, OutEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
    }
    events
}

fn output(events: &[OutEvent], request_id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                request_id: rid,
                output,
                is_error,
                ..
            } if rid == request_id => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no ToolOutput for {request_id}: {events:?}"))
}

#[tokio::test]
async fn discovery_tools_answer_under_a_deny_all_mode_without_parking() {
    let events = run_calls(&[
        ("list_agents", "{}"),
        ("list_models", "{}"),
        ("list_skills", "{}"),
        ("list_mcp", "{}"),
        ("mcp_tools", "{}"),
    ])
    .await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a discovery read never parks an approval: {events:?}"
    );

    let (agents, err) = output(&events, "c0");
    assert!(!err && agents.starts_with("Agents ("), "{agents}");
    assert!(agents.contains("general —"), "{agents}");

    let (models, err) = output(&events, "c1");
    assert!(
        !err && models.contains("local/tiny — Tiny Model"),
        "{models}"
    );

    let (skills, err) = output(&events, "c2");
    assert!(!err && skills.contains("git — commit helpers"), "{skills}");

    let (mcp, err) = output(&events, "c3");
    assert!(!err, "{mcp}");
    assert_eq!(mcp, "No MCP servers are configured.");

    let (bad, err) = output(&events, "c4");
    assert!(err, "a missing name is a tool error: {bad}");
    assert!(bad.contains("mcp_tools expects {\"name\""), "{bad}");
}

/// The same deny-all mode still refuses a *graded* call — the discovery
/// bypass is by capability, not a blanket hole.
#[tokio::test]
async fn a_graded_call_is_still_denied_under_the_same_mode() {
    let events = run_calls(&[("read", r#"{"path":"x"}"#)]).await;
    let (out, err) = output(&events, "c0");
    assert!(err, "{out}");
    assert!(out.contains("denied by mode `build`"), "{out}");
}
