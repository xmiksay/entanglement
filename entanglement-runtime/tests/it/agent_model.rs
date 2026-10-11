//! Integration tests for `agent`'s `model` parameter (#560 P12, ADR-0207
//! §12): a spawning model may pick the child's model, validated against the
//! catalog before any child is minted. Drives the real tool executor with a
//! real catalog wired (`ToolExecutor::builder` +
//! `AdvertisingInputs`, mirroring `advertising_pin.rs`'s harness) so
//! `resolve_model` sees genuine catalog data, not a stub.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Catalog, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest,
    LlmResponse, LlmStream, MessageRole, OutEvent, Permission, PermissionProfile, ResolvedModel,
    SessionId, ToolCall,
};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_advertising::AdvertisingInputs;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::ToolRegistry;

const CATALOG: &str = "providers:\n\
  \x20 - name: zai\n\
  \x20   default_model: glm-5.3\n\
  \x20   models:\n\
  \x20     - id: glm-5.3\n\
  \x20 - name: openai\n\
  \x20   default_model: gpt-4o\n\
  \x20   models:\n\
  \x20     - id: gpt-4o\n";

fn finish(text: &str) -> LlmStream {
    stream_from_response(LlmResponse {
        text: text.into(),
        tool_calls: vec![],
        ..Default::default()
    })
}

fn call(id: &str, name: &str, input: String) -> LlmStream {
    stream_from_response(LlmResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input,
            provider_meta: None,
        }],
        ..Default::default()
    })
}

fn last_tool<'a>(req: &'a LlmRequest<'_>) -> Option<&'a str> {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Tool)
        .and_then(|m| m.content.iter().find_map(|p| p.as_text()))
}

/// The parent launches one blocking `agent` call carrying `model`; once it
/// sees the tool result (success or refusal) it finishes. The child, if one
/// starts, answers directly on its first turn.
struct SpawnModelLlm {
    model_arg: &'static str,
}

#[async_trait]
impl Llm for SpawnModelLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        if req.messages.iter().any(|m| m.role == MessageRole::User)
            && last_tool(&req).is_none()
            && req.messages.iter().any(|m| m.text() == "child-task")
        {
            return Ok(finish("child-answer"));
        }
        match last_tool(&req) {
            Some(_) => Ok(finish("parent done")),
            None => Ok(call(
                "spawn1",
                "agent",
                format!(
                    r#"{{"agent":"general","prompt":"child-task","model":"{}"}}"#,
                    self.model_arg
                ),
            )),
        }
    }
}

fn engine_config(model_arg: &'static str) -> EngineConfig {
    let resolved_factory: LlmFactory =
        Arc::new(move || Box::new(SpawnModelLlm { model_arg }) as Box<dyn Llm>);
    EngineConfig {
        llm_factory: Arc::new(move || Box::new(SpawnModelLlm { model_arg }) as Box<dyn Llm>),
        agents: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        model_resolver: Some(Arc::new(move |_user, provider: &str, model: &str| {
            Ok(ResolvedModel {
                provider: provider.into(),
                model: model.into(),
                llm_factory: resolved_factory.clone(),
                generation: None,
                context_window: None,
            })
        })),
        ..EngineConfig::default()
    }
}

/// Wire the real executor with a genuine catalog (`AdvertisingInputs`), a
/// permissive single-mode table, and no advertising/system-prompt resolvers
/// — this test only cares about `agent`'s `model` validation, not what gets
/// advertised.
fn load_config() -> entanglement_runtime::config::Config {
    // No user layer: never the real `config.yml` of whoever runs the test.
    let dir = tempfile::tempdir().expect("tempdir");
    entanglement_runtime::config::Config::load_with(dir.path(), None).expect("config loads")
}

fn spawn_executor(
    holly: &Holly,
    agents: entanglement_core::AgentCatalog,
) -> tokio::task::JoinHandle<()> {
    let catalog: Catalog = serde_yaml::from_str(CATALOG).expect("catalog yaml");
    let inputs = Arc::new(AdvertisingInputs::new(
        Arc::new(load_config()),
        Some(Arc::new(catalog)),
    ));
    let tools = ToolRegistry::new().shared();
    let base = PermissionProfile::new(Permission::Allow);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        crate::mode_support::allow_all_table(),
        tools.clone(),
        base.clone(),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load_from(None));
    ToolExecutor::builder(holly, tools)
        .agents(agents)
        .base(base)
        .active(active)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(Arc::new(
            entanglement_runtime::mode::ModeTable::builtin()
                .expect("built-in permission modes must parse"),
        ))
        .advertising_inputs(inputs)
        .spawn()
}

#[tokio::test]
async fn unknown_model_refuses_the_spawn_and_names_every_valid_id() {
    let cfg = engine_config("bogus-model");
    let profiles = cfg.agents.clone();
    let holly = Holly::spawn(cfg);
    let _executor = spawn_executor(&holly, profiles);

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "parent-task"))
        .await
        .unwrap();

    let mut child_started = false;
    let mut refusal_text = String::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                parent: Some(p), ..
            } if p == &parent => child_started = true,
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                is_error,
                ..
            } if session == &parent && tool == "agent" => {
                refusal_text = output.clone();
                assert!(*is_error, "an unknown model id must set is_error");
            }
            OutEvent::Done { session, .. } if session == &parent => break,
            _ => {}
        }
    }

    assert!(
        !child_started,
        "an unknown model id must refuse before any child is minted"
    );
    assert!(refusal_text.contains("unknown model"), "{refusal_text}");
    assert!(refusal_text.contains("glm-5.3"), "{refusal_text}");
    assert!(refusal_text.contains("gpt-4o"), "{refusal_text}");
}

#[tokio::test]
async fn valid_model_rebinds_the_childs_session_before_its_first_turn() {
    let cfg = engine_config("gpt-4o");
    let profiles = cfg.agents.clone();
    let holly = Holly::spawn(cfg);
    let _executor = spawn_executor(&holly, profiles);

    let parent = SessionId::new("parent");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(parent.clone(), "parent-task"))
        .await
        .unwrap();

    let mut child: Option<SessionId> = None;
    let mut saw_model_changed = false;
    let mut parent_answer = String::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        match &ev {
            OutEvent::SessionStarted {
                session,
                parent: Some(p),
                ..
            } if p == &parent => child = Some(session.clone()),
            OutEvent::ModelChanged { session, model, .. }
                if Some(session) == child.as_ref() && model == "gpt-4o" =>
            {
                saw_model_changed = true;
            }
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if session == &parent && tool == "agent" => {
                parent_answer = output.clone();
            }
            OutEvent::Done { session, .. } if session == &parent => break,
            _ => {}
        }
    }

    assert!(child.is_some(), "a child session must have started");
    assert!(
        saw_model_changed,
        "a valid model id must rebind the child's session via SetModel"
    );
    assert!(parent_answer.contains("child-answer"), "{parent_answer}");
}
