//! The #934 scenario, shared by the offline guard (`it::tool_surface_guard`)
//! and its live variant (`tests/live_tool_surface.rs`): 300 registered deep
//! tools in six groups, each group a host script namespace behind one skill,
//! wired exactly like an embedder would — production surface and system-prompt
//! resolvers, the real executor, the mode resolver.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    AgentCatalog, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest, LlmStream, OutEvent,
    Permission, SessionId,
};
use entanglement_runtime::capability::Capability;
use entanglement_runtime::host_ns::{HostFn, HostNamespace};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::skills::{LoadSkillTool, SkillMeta, SkillRegistry};
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::AdvertisingState;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
use entanglement_runtime::tools::CallGrading;
use entanglement_runtime::{system_prompt_mode, Tool, ToolRegistry};

/// The skills, each grouping 50 tools: `<skill>_<verb>_<noun>`, called from a
/// script as `<skill>::<verb>_<noun>(#{...})`.
pub const SKILLS: [(&str, [&str; 5]); 6] = [
    (
        "billing",
        ["invoice", "payment", "refund", "subscription", "tax_rate"],
    ),
    ("crm", ["contact", "company", "deal", "note", "pipeline"]),
    (
        "inventory",
        ["stock_level", "warehouse", "sku", "reservation", "supplier"],
    ),
    (
        "shipping",
        ["shipment", "carrier", "label", "route", "parcel"],
    ),
    ("support", ["ticket", "macro", "shift", "sla", "survey"]),
    (
        "analytics",
        ["dashboard", "metric", "report", "segment", "funnel"],
    ),
];

const VERBS: [&str; 10] = [
    "get", "list", "search", "count", "export", "create", "update", "delete", "archive", "restore",
];

/// Every registered deep tool's name, in registration order.
pub fn deep_tool_names() -> Vec<String> {
    SKILLS
        .iter()
        .flat_map(|(skill, nouns)| {
            nouns.iter().flat_map(move |noun| {
                VERBS
                    .iter()
                    .map(move |verb| format!("{skill}_{verb}_{noun}"))
            })
        })
        .collect()
}

/// Every deep tool that actually ran, by name, in call order.
pub type CallLog = Arc<Mutex<Vec<String>>>;

/// One deep tool: a real registered [`Tool`] with its own description and
/// schema, never advertised.
struct DeepTool {
    name: String,
    description: String,
    read_only: bool,
    log: CallLog,
}

#[async_trait]
impl Tool for DeepTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The record id." },
                "sku": { "type": "string", "description": "A stock-keeping unit code." },
                "warehouse": { "type": "string", "description": "A warehouse code." },
                "fields": { "type": "object", "description": "Fields to set." }
            }
        })
    }
    fn capabilities(&self) -> &'static [Capability] {
        if self.read_only {
            &[Capability::Read]
        } else {
            &[Capability::Write]
        }
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        self.log.lock().unwrap().push(self.name.clone());
        let args: serde_json::Value = serde_json::from_str(input)?;
        Ok(serde_json::json!({ "tool": self.name, "args": args, "on_hand": 42 }).to_string())
    }
}

/// A group's host-namespace backing tool: `{"tool": "<fn>", "args": {...}}`
/// runs the registered deep tool `<skill>_<fn>`, graded as that tool.
struct GroupTool {
    name: String,
    skill: &'static str,
    targets: BTreeMap<String, Arc<DeepTool>>,
}

impl GroupTool {
    fn target(&self, input: &str) -> Option<(&Arc<DeepTool>, serde_json::Value)> {
        let v: serde_json::Value = serde_json::from_str(input).ok()?;
        let target = self.targets.get(v.get("tool")?.as_str()?)?;
        Some((target, v.get("args").cloned().unwrap_or_default()))
    }
}

#[async_trait]
impl Tool for GroupTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }
    fn description(&self) -> &str {
        "dispatch one call to a group's deep tool"
    }
    fn call_grading(&self, input: &str) -> Option<CallGrading> {
        let (target, _) = self.target(input)?;
        Some(CallGrading {
            name: target.name.clone(),
            capabilities: target.capabilities(),
        })
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        let Some((target, args)) = self.target(input) else {
            anyhow::bail!("{}: unknown function in {input}", self.skill);
        };
        target.run(&args.to_string()).await
    }
}

/// The skill grouping one namespace: its body is the function list
/// `load_skill` returns.
fn skill(name: &str, nouns: &[&str]) -> SkillMeta {
    let mut body = format!(
        "Call these from ONE `rhai` script as `{name}::<fn>(#{{...}})`; \
         each returns JSON text (parse_json it):\n"
    );
    for noun in nouns {
        for verb in VERBS {
            body.push_str(&format!(
                "- {name}::{verb}_{noun}(#{{id, sku, warehouse, fields}}) — {verb} a {noun}\n"
            ));
        }
    }
    let things = nouns.join(", ").replace('_', " ");
    SkillMeta::new(
        name,
        format!("{name}: {} tools over {things}", nouns.len() * VERBS.len()),
        body,
    )
}

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

/// Register one group's 50 deep tools; return its backing tool and namespace.
fn group(
    skill: &'static str,
    nouns: &[&str],
    log: &CallLog,
    reg: &mut ToolRegistry,
) -> (GroupTool, HostNamespace) {
    let mut targets = BTreeMap::new();
    for noun in nouns {
        for (i, verb) in VERBS.iter().enumerate() {
            let tool = Arc::new(DeepTool {
                name: format!("{skill}_{verb}_{noun}"),
                description: format!(
                    "{verb} a {noun} record in {skill}: id selects it; fields carries \
                     the values to set; returns the record as JSON."
                ),
                read_only: i < 5,
                log: log.clone(),
            });
            reg.register_arc(tool.clone());
            targets.insert(format!("{verb}_{noun}"), tool);
        }
    }
    let fns: Vec<HostFn> = targets
        .iter()
        .map(|(name, tool)| HostFn {
            name: name.clone(),
            description: tool.description.clone(),
        })
        .collect();
    let backing = format!("{skill}_invoke");
    let ns = HostNamespace {
        name: skill.into(),
        tool: backing.clone(),
        resolver: Arc::new(move |_| Some(fns.clone())),
    };
    (
        GroupTool {
            name: backing,
            skill,
            targets,
        },
        ns,
    )
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
