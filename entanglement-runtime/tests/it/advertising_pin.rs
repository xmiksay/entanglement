//! ADR-0196 §2 / ADR-0204: a session's advertised tools array and system
//! prompt are byte-identical from round 1 on. The mode/encoding/discovery pin
//! used to be taken when the executor saw `SessionStarted`, a broadcast event
//! core's first round does not wait for — round 1 could resolve the unpinned
//! default and round 2 the pinned value, a full prompt-cache miss. These run
//! the production resolvers (`tool_advertising::surface`,
//! `system_prompt_mode`) against a real engine + executor. Also pinned here:
//! enabling a tool never changes the array — only a delivered schema does —
//! and a `Full` session's array never shrinks when a tool disappears.

use std::borrow::Cow;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Catalog, Discovery, EngineConfig, Holly, InMsg, Llm, LlmFactory,
    LlmRequest, LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ResolvedModel,
    SessionId, ToolAdvertising, ToolCall, ToolOverlayEntry,
};
use entanglement_runtime::config::Config;
use entanglement_runtime::mcp::AvailableMcp;
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::{AdvertisingInputs, AdvertisingState};
use entanglement_runtime::tool_runner::{spawn_tool_executor_with_policy, DiscoverySurface};
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

const CATALOG: &str = "providers:\n\
  \x20 - name: tail\n\
  \x20   default_model: t\n\
  \x20   models:\n\
  \x20     - id: t\n\
  \x20 - name: p\n\
  \x20   default_model: nf\n\
  \x20   discovery: native_first\n\
  \x20   models:\n\
  \x20     - id: nf\n\
  \x20     - id: fullm\n\
  \x20       tool_advertising: full\n";

/// An engine whose `build` profile pins `<provider>/<model>`, wired exactly
/// like the binary: production resolvers + executor over one
/// `AdvertisingState`, with `read` (kernel), `glob` and `gone` registered.
pub struct Harness {
    pub holly: Holly,
    pub rec: Recorded,
    pub advertising: Arc<AdvertisingState>,
    pub tools: SharedRegistry,
    pub sid: SessionId,
    _executor: tokio::task::JoinHandle<()>,
}

fn load_config() -> Config {
    let _env = crate::env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("ENTANGLEMENT_CONFIG_FILE", dir.path().join("absent.yml"));
    let config = Config::load(dir.path()).expect("config loads");
    std::env::remove_var("ENTANGLEMENT_CONFIG_FILE");
    config
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
    let mut build = profiles.get("build").cloned().expect("build profile");
    build.provider = Some(provider.into());
    build.model = Some(model.into());
    profiles.insert(build);

    let advertising = Arc::new(AdvertisingState::new());
    let mut reg = ToolRegistry::new();
    for name in ["read", "glob", "gone"] {
        reg.register(Echo(name));
    }
    let tools = reg.shared();
    let resolved_factory = factory.clone();
    let catalog: Catalog = serde_yaml::from_str(CATALOG).expect("catalog yaml");
    let inputs = Arc::new(AdvertisingInputs::new(
        Arc::new(load_config()),
        Some(Arc::new(catalog)),
    ));
    let holly = Holly::spawn(EngineConfig {
        llm_factory: factory,
        profiles: profiles.clone(),
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
            avail: Arc::new(AvailableMcp::default()),
            advertising: advertising.clone(),
            inputs: inputs.clone(),
        })),
        system_prompt_resolver: Some(system_prompt_mode::resolver(advertising.clone())),
        ..EngineConfig::default()
    });

    let base = PermissionProfile::new(Permission::Allow);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        perm_modes.clone(),
        crate::mode_support::allow_all_table(),
        tools.clone(),
        base.clone(),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let executor = spawn_tool_executor_with_policy(
        &holly,
        tools.clone(),
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
        Arc::new(
            entanglement_runtime::mode::ModeTable::builtin()
                .expect("built-in permission modes must parse"),
        ),
        Arc::new(PlanFileRegistry::new()),
        None,
        Some(inputs),
        Some(DiscoverySurface {
            advertising: advertising.clone(),
            ..Default::default()
        }),
    );
    Harness {
        holly,
        rec,
        advertising,
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
    }
}

pub fn text() -> LlmResponse {
    LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    }
}

/// Round 1 calls `read`, round 2 answers: both requests must carry the same
/// bytes.
async fn first_two_rounds(model: &str) -> Harness {
    let h = harness("p", model, vec![text(), call("read", r#"{"path":"x"}"#)]);
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
async fn native_first_session_is_byte_stable_from_round_one() {
    let h = first_two_rounds("nf").await;
    assert_eq!(h.advertising.discovery(&h.sid), Discovery::NativeFirst);
    let rec = h.requests();
    assert!(
        rec[0].names.contains(&"invoke".to_string()),
        "{:?}",
        rec[0].names
    );
    assert!(rec[0].system.contains("only if you cannot, call invoke"));
}

#[tokio::test]
async fn full_session_is_byte_stable_from_round_one() {
    let h = first_two_rounds("fullm").await;
    assert_eq!(h.advertising.mode(&h.sid), ToolAdvertising::Full);
    let rec = h.requests();
    assert!(!rec[0].system.contains("use explore to search them"));
}

#[tokio::test]
async fn append_overlay_enable_changes_nothing_until_describe_delivers_the_schema() {
    let h = harness(
        "tail",
        "t",
        vec![text(), call("describe", r#"{"names":["glob"]}"#), text()],
    );
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
    assert_eq!(rec.len(), 3, "{rec:#?}");
    assert_eq!(rec[0], rec[1], "an overlay enable changed the request");
    let mut want = rec[1].names.clone();
    want.push("glob".into());
    assert_eq!(
        rec[2].names, want,
        "describe appends the tool once, at the end"
    );
}

#[tokio::test]
async fn full_array_survives_a_removed_tool_whose_call_declines() {
    let h = harness("p", "fullm", vec![text(), call("gone", "{}"), text()]);
    h.turn("one").await;
    h.tools.write().unwrap().unregister("gone");
    let events = h.turn("two").await;
    let rec = h.requests();
    assert_eq!(rec.len(), 3, "{rec:#?}");
    assert!(rec[0].names.contains(&"gone".to_string()));
    assert_eq!(rec[0].tools, rec[1].tools);
    assert_eq!(rec[1].tools, rec[2].tools);
    let is_error = events.iter().find_map(|e| match e {
        OutEvent::ToolOutput { is_error, .. } => Some(*is_error),
        _ => None,
    });
    assert_eq!(is_error, Some(true), "{events:#?}");
}
