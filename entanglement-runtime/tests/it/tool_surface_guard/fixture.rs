//! The #934 scenario, shared by the offline guard (`it::tool_surface_guard`)
//! and its live variant (`tests/live_tool_surface.rs`): 300 registered deep
//! tools in six groups (`deep_tools`), each group a host script namespace
//! behind one skill, wired exactly like an embedder would — production
//! surface and system-prompt resolvers, the real executor, the mode resolver.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    AgentCatalog, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest, LlmStream, OutEvent,
    Permission, SessionId,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::skills::{LoadSkillTool, SkillRegistry};
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::AdvertisingState;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
use entanglement_runtime::{system_prompt_mode, ToolRegistry};

pub use super::deep_tools::deep_tool_names;
use super::deep_tools::{group, skill, CallLog, SKILLS};

/// A one-mode (`build`) table: default allow, with `deny`/`prompt` rules
/// naming deep tools.
pub fn mode_table(deny: &[&str], prompt: &[&str]) -> Arc<ModeTable> {
    let owned = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mode = Mode {
        name: "build".into(),
        default: Permission::Allow,
        rules: Rules::from_lists(&owned(deny), &[], &owned(prompt)),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("one-mode table is valid"))
}

/// The first `ToolSpec` array and system prompt the provider saw, plus how
/// many requests it got.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub requests: usize,
    pub first_names: Vec<String>,
    /// Serialized bytes of the first request's tools array: every spec's
    /// name, description and JSON schema.
    pub first_tool_bytes: usize,
    pub first_system: String,
}

struct Recording {
    inner: Box<dyn Llm>,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait]
impl Llm for Recording {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        {
            let mut seen = self.seen.lock().unwrap();
            if seen.requests == 0 {
                seen.first_names = req.tools.iter().map(|t| t.name.clone()).collect();
                seen.first_tool_bytes = req
                    .tools
                    .iter()
                    .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
                    .sum();
                seen.first_system = req.system.to_string();
            }
            seen.requests += 1;
        }
        self.inner.stream(req).await
    }
}

/// One engine + executor over the scenario.
pub struct Scenario {
    pub holly: Holly,
    pub calls: CallLog,
    pub seen: Arc<Mutex<Seen>>,
    _executor: tokio::task::JoinHandle<()>,
}

/// Wire the scenario over `llm`: `with_deep_tools` registers the 300 tools
/// and their namespaces (off, the same engine with none), `system_prompt` is
/// the `general` agent's.
pub fn spawn(
    llm: LlmFactory,
    modes: Arc<ModeTable>,
    with_deep_tools: bool,
    system_prompt: &str,
) -> Scenario {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let calls: CallLog = Arc::default();
    let factory: LlmFactory = {
        let seen = seen.clone();
        Arc::new(move || {
            Box::new(Recording {
                inner: llm(),
                seen: seen.clone(),
            }) as Box<dyn Llm>
        })
    };

    let mut reg = ToolRegistry::new();
    let mut skills = SkillRegistry::default();
    let mut namespaces = Vec::new();
    if with_deep_tools {
        for (name, nouns) in SKILLS {
            let (backing, ns) = group(name, &nouns, &calls, &mut reg);
            reg.register(backing);
            namespaces.push(ns);
            skills.insert(skill(name, &nouns));
        }
    }
    let skills = Arc::new(RwLock::new(Arc::new(skills)));
    reg.register(LoadSkillTool::new(skills.clone()));
    let tools = reg.shared();

    let mut agents = AgentCatalog::default();
    agents.insert(entanglement_core::Agent {
        name: "general".into(),
        description: "the general agent".into(),
        system_prompt: system_prompt.into(),
        model: None,
        provider: None,
        default_mode: None,
    });
    let advertising = Arc::new(AdvertisingState::new());
    let holly = Holly::spawn(EngineConfig {
        llm_factory: factory,
        agents: agents.clone(),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: tools.clone(),
            advertising: advertising.clone(),
            agent_specs: entanglement_runtime::subagent::agent_specs(),
        })),
        system_prompt_resolver: Some(system_prompt_mode::resolver(advertising.clone())),
        ..EngineConfig::default()
    });
    let mut builder = ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .mode_table(modes)
        .live_skills(skills)
        .discovery(DiscoverySurface {
            advertising,
            ..Default::default()
        });
    for ns in namespaces {
        builder = builder.host_namespace(ns);
    }
    Scenario {
        holly,
        calls,
        seen,
        _executor: builder.spawn(),
    }
}

impl Scenario {
    /// Prompt `sid` and collect its events up to `Done`, approving every
    /// card when `approve` (else leaving it parked, which fails the wait).
    pub async fn turn(&self, sid: &SessionId, text: &str, approve: bool) -> Vec<OutEvent> {
        self.turn_within(sid, text, approve, Duration::from_secs(10))
            .await
            .unwrap_or_else(|events| panic!("turn did not finish: {events:#?}"))
    }

    /// [`Self::turn`] bounded by `idle`: `Err` carries what arrived before
    /// the session went quiet.
    pub async fn turn_within(
        &self,
        sid: &SessionId,
        text: &str,
        approve: bool,
        idle: Duration,
    ) -> Result<Vec<OutEvent>, Vec<OutEvent>> {
        let mut sub = self.holly.subscribe();
        self.holly
            .send(InMsg::prompt(sid.clone(), text))
            .await
            .expect("engine accepts the prompt");
        let mut events = Vec::new();
        loop {
            match tokio::time::timeout(idle, sub.recv()).await {
                Ok(Ok(ev)) if ev.session() == Some(sid) => {
                    if let (true, OutEvent::ToolRequest { request_id, .. }) = (approve, &ev) {
                        self.holly
                            .send(InMsg::Approve {
                                session: sid.clone(),
                                request_id: request_id.clone(),
                                scope: Default::default(),
                                mode: None,
                                run_all: false,
                            })
                            .await
                            .expect("engine accepts the approval");
                    }
                    let done = matches!(ev, OutEvent::Done { .. });
                    events.push(ev);
                    if done {
                        return Ok(events);
                    }
                }
                Ok(Ok(_)) => {}
                _ => return Err(events),
            }
        }
    }
}
