//! ADR-0226: one advertised surface, pinned for the session — a session's
//! tools array and system prompt are byte-identical from round 1 on, on every
//! wire, whatever is registered, removed, or enabled later. These run the
//! production resolvers (`tool_advertising::surface`, `system_prompt_mode`)
//! against a real engine + executor.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, ResolvedModel, SessionId, ToolCall,
    ToolOverlayEntry,
};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::AdvertisingState;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
use entanglement_runtime::{system_prompt_mode, SharedRegistry, Tool, ToolRegistry};

/// One LLM request as the provider would see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Req {
    pub names: Vec<String>,
    pub tools: String,
    pub system: String,
}

/// Every LLM request, in arrival order.
pub type Recorded = Arc<Mutex<Vec<Req>>>;

struct Recording {
    rec: Recorded,
    script: Arc<Mutex<Vec<LlmResponse>>>,
}

#[async_trait]
impl Llm for Recording {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.rec.lock().unwrap().push(Req {
            names: req.tools.iter().map(|t| t.name.clone()).collect(),
            tools: format!("{:?}", req.tools),
            system: req.system.to_string(),
        });
        let resp = self.script.lock().unwrap().pop().unwrap_or(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
            ..Default::default()
        });
        Ok(stream_from_response(resp))
    }
}

struct Echo(&'static str);
#[async_trait]
impl Tool for Echo {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

/// An engine whose `general` profile pins `<provider>/<model>`, wired exactly
/// like the binary: production resolvers + executor over one
/// `AdvertisingState`, with `read` (kernel), `glob` (hidden) and `gone`
/// registered.
pub struct Harness {
    pub holly: Holly,
    pub rec: Recorded,
    pub tools: SharedRegistry,
    pub sid: SessionId,
    _executor: tokio::task::JoinHandle<()>,
}

pub fn harness(provider: &str, model: &str, script: Vec<LlmResponse>) -> Harness {
    let rec: Recorded = Arc::default();
    let script = Arc::new(Mutex::new(script));
    let factory: LlmFactory = {
        let (rec, script) = (rec.clone(), script.clone());
        Arc::new(move || {
            Box::new(Recording {
                rec: rec.clone(),
                script: script.clone(),
            }) as Box<dyn Llm>
        })
    };
    let mut profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let mut general = profiles.get("general").cloned().expect("general profile");
    general.provider = Some(provider.into());
    general.model = Some(model.into());
    profiles.insert(general);

    let advertising = Arc::new(AdvertisingState::new());
    let mut reg = ToolRegistry::new();
    for name in ["read", "glob", "gone"] {
        reg.register(Echo(name));
    }
    let tools = reg.shared();
    let resolved_factory = factory.clone();
    let holly = Holly::spawn(EngineConfig {
        llm_factory: factory,
        agents: profiles.clone(),
        model_resolver: Some(Arc::new(move |_user, provider: &str, model: &str| {
            Ok(ResolvedModel {
                provider: provider.into(),
                model: model.into(),
                llm_factory: resolved_factory.clone(),
                generation: None,
                context_window: None,
            })
        })),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: tools.clone(),
            advertising: advertising.clone(),
            // Mirror `boot.rs`: the spawn specs reach the model through the
            // resolver, never `cfg.tool_specs`, which the resolver replaces.
            agent_specs: entanglement_runtime::subagent::agent_specs(),
        })),
        system_prompt_resolver: Some(system_prompt_mode::resolver(advertising.clone())),
        ..EngineConfig::default()
    });

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
    let executor = ToolExecutor::builder(&holly, tools.clone())
        .agents(profiles)
        .base(base)
        .active(active)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(Arc::new(
            entanglement_runtime::mode::ModeTable::builtin()
                .expect("built-in permission modes must parse"),
        ))
        .discovery(DiscoverySurface {
            advertising,
            ..Default::default()
        })
        .spawn();
    Harness {
        holly,
        rec,
        tools,
        sid: SessionId::new("s1"),
        _executor: executor,
    }
}

impl Harness {
    /// Send a prompt and collect this session's events up to its `Done`.
    pub async fn turn(&self, text: &str) -> Vec<OutEvent> {
        let mut sub = self.holly.subscribe();
        self.holly
            .send(InMsg::prompt(self.sid.clone(), text))
            .await
            .unwrap();
        let mut events = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
                Ok(Ok(ev)) if ev.session() == Some(&self.sid) => {
                    let done = matches!(ev, OutEvent::Done { .. });
                    events.push(ev);
                    if done {
                        return events;
                    }
                }
                Ok(Ok(_)) => {}
                other => panic!("turn did not finish: {other:?}"),
            }
        }
    }

    pub fn requests(&self) -> Vec<Req> {
        self.rec.lock().unwrap().clone()
    }
}

pub fn call(name: &str, input: &str) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: format!("call-{name}"),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

pub fn text() -> LlmResponse {
    LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
        ..Default::default()
    }
}

/// Round 1 calls `read`, round 2 answers: both requests must carry the same
/// bytes.
async fn first_two_rounds(provider: &str, model: &str) -> Harness {
    let h = harness(
        provider,
        model,
        vec![text(), call("read", r#"{"path":"x"}"#)],
    );
    h.turn("go").await;
    let rec = h.requests();
    assert_eq!(rec.len(), 2, "{rec:#?}");
    assert_eq!(
        rec[0].tools, rec[1].tools,
        "tools array changed between rounds"
    );
    assert_eq!(
        rec[0].system, rec[1].system,
        "system prompt changed between rounds"
    );
    h
}

#[tokio::test]
async fn a_session_is_byte_stable_from_round_one() {
    first_two_rounds("zai", "glm-5.3").await;
}

/// No mode, no per-wire encoding: an Anthropic-bound and an OpenAI-bound
/// session advertise the very same bytes.
#[tokio::test]
async fn every_wire_gets_the_same_array() {
    let a = first_two_rounds("anthropic", "claude-sonnet-5").await;
    let b = first_two_rounds("openai", "gpt-4o").await;
    assert_eq!(a.requests()[0].tools, b.requests()[0].tools);
}

/// The rendered round-1 request carries exactly the kernel: the `mcp`
/// envelope and the discovery tools in, the retired surface and the
/// script-only tools out.
#[tokio::test]
async fn round_one_advertises_the_kernel_only() {
    let h = harness("zai", "glm-5.3", vec![text()]);
    h.turn("one").await;
    let names = &h.requests()[0].names;
    for present in [
        "read",
        "mcp",
        "list_agents",
        "list_models",
        "list_skills",
        "list_mcp",
        "mcp_tools",
        "agent",
        "poll",
    ] {
        assert!(names.iter().any(|n| n == present), "{present}: {names:?}");
    }
    for absent in [
        "explore",
        "describe",
        "invoke",
        "glob",
        "gone",
        "mcp_enable",
    ] {
        assert!(!names.iter().any(|n| n == absent), "{absent}: {names:?}");
    }
}

#[tokio::test]
async fn an_overlay_enable_never_changes_the_array() {
    let h = harness("zai", "glm-5.3", vec![text(), text()]);
    h.turn("one").await;
    let mut sub = h.holly.subscribe();
    h.holly
        .send(InMsg::SetToolOverlay {
            session: h.sid.clone(),
            entries: vec![ToolOverlayEntry::allow("glob")],
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolOverlayChanged { .. })) => break,
            Ok(Ok(_)) => {}
            other => panic!("no overlay confirmation: {other:?}"),
        }
    }
    h.turn("two").await;
    let rec = h.requests();
    assert_eq!(rec.len(), 2, "{rec:#?}");
    assert_eq!(rec[0], rec[1], "an overlay enable changed the request");
}

/// A tool registered mid-session (an MCP server connecting) or removed never
/// moves the cached array; a call to the removed one declines.
#[tokio::test]
async fn registry_changes_never_move_the_array() {
    let h = harness("zai", "glm-5.3", vec![text(), call("gone", "{}"), text()]);
    h.turn("one").await;
    {
        let mut tools = h.tools.write().unwrap();
        tools.unregister("gone");
        tools.register(Echo("mcp__kb__search"));
        tools.register(Echo("write"));
    }
    let events = h.turn("two").await;
    let rec = h.requests();
    assert_eq!(rec.len(), 3, "{rec:#?}");
    assert_eq!(rec[0].tools, rec[1].tools);
    assert_eq!(rec[1].tools, rec[2].tools);
    let is_error = events.iter().find_map(|e| match e {
        OutEvent::ToolOutput { is_error, .. } => Some(*is_error),
        _ => None,
    });
    assert_eq!(is_error, Some(true), "{events:#?}");
}
