//! ADR-0204: a `ToolExec` still named `invoke` reaches the runtime only when
//! core declined to unwrap it. In a session whose discovery strategy
//! advertises the envelope it is declined with the envelope's schema, on
//! both the mask-miss path and `dispatch`; anywhere else it is an ordinary
//! unknown tool. Core here has no `tool_spec_resolver`, so it never unwraps.

use std::borrow::Cow;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Discovery, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolAdvertising, ToolCall,
};
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver, SandboxConfig,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_advertising::{AdvertisingState, Encoding};
use entanglement_runtime::tool_runner::{spawn_tool_executor_with_policy, DiscoverySurface};
use entanglement_runtime::{Tool, ToolRegistry};

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.responses.lock().unwrap().pop().unwrap_or(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        });
        Ok(stream_from_response(resp))
    }
}

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

/// A session `s1` pinned to `ToolSearch` + `client_side` + `discovery`, on
/// `agent`, whose scripted model makes one malformed `invoke` call.
async fn run_malformed_invoke(discovery: Discovery, agent: &str) -> Vec<OutEvent> {
    let call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "invoke".into(),
            input: r#"{"nme":"grep","args":"nope"}"#.into(),
            provider_meta: None,
        }],
    };
    let ok = LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let scripted = Arc::new(vec![ok, call]);
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: Mutex::new((*scripted).clone()),
            }) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    });

    let sid = SessionId::new("s1");
    let advertising = Arc::new(AdvertisingState::new());
    {
        let mut modes = advertising.modes.lock().unwrap();
        modes.pin(
            sid.clone(),
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
        );
        modes.set_discovery(&sid, discovery);
    }
    let mut reg = ToolRegistry::new();
    reg.register(EchoRead);
    let base = PermissionProfile::new(Permission::Allow);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let shared_tools = reg.shared();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        perm_modes.clone(),
        crate::mode_support::allow_all_table(),
        shared_tools.clone(),
        base.clone(),
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
        base,
        active,
        perm_modes,
        resolver,
        grants,
        Default::default(),
        None,
        SandboxConfig::none(),
        Arc::new(PlanFileRegistry::new()),
        None,
        // No advertising inputs: nothing re-pins over the state set above.
        None,
        Some(DiscoverySurface {
            advertising,
            ..Default::default()
        }),
    );

    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: agent.into(),
        })
        .await
        .unwrap();
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
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

/// The single `ToolOutput`'s text and `is_error`, asserting nothing parked.
fn decline(events: &[OutEvent]) -> (String, bool) {
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a stray invoke must never park an approval; got {events:?}"
    );
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a ToolOutput; got {events:?}"))
}

#[tokio::test]
async fn malformed_envelope_under_invoke_explains_the_schema_on_dispatch() {
    // `build` admits every tool, so the registry miss is found in `dispatch`.
    let (output, is_error) = decline(&run_malformed_invoke(Discovery::Invoke, "build").await);
    assert!(output.starts_with("malformed invoke call"), "{output}");
    assert!(output.contains(r#""args": {...}"#), "{output}");
    assert!(is_error);
}

#[tokio::test]
async fn malformed_envelope_under_native_first_explains_the_schema_on_a_mask_miss() {
    // `explore`'s allowlist doesn't name `invoke`: the mask-miss path.
    let (output, is_error) =
        decline(&run_malformed_invoke(Discovery::NativeFirst, "explore").await);
    assert!(output.starts_with("malformed invoke call"), "{output}");
    assert!(is_error);
}

#[tokio::test]
async fn stray_invoke_under_append_is_an_ordinary_unknown_tool() {
    for agent in ["build", "explore"] {
        let (output, is_error) = decline(&run_malformed_invoke(Discovery::Append, agent).await);
        assert!(
            output.starts_with("unknown tool: `invoke`"),
            "{agent}: {output}"
        );
        assert!(is_error);
    }
}
