//! A session whose model makes a scripted series of tool calls (`rhai`, or
//! any host tool via [`run_calls`]), wired the
//! way `engine_setup` wires the executor (real grants, mode resolver,
//! optional escape-root policy, hooks, overlay, ceiling, workdir,
//! `rhai.trusted`, credentialed `http()`) — the shared harness of the ADR-0228 launch tests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use entanglement_core::{
    ApprovalScope, CardReason, EngineConfig, Holly, InMsg, Llm, LlmResponse, OutEvent, Permission,
    PermissionProfile, SessionId, ToolCall, ToolOverlayEntry,
};
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::hooks::Hooks;
use entanglement_runtime::host::{host_tools_with, HostToolOptions};
use entanglement_runtime::http_gate::HttpGate;
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::rhai_trust::TrustedFolders;
use entanglement_runtime::skills::{LoadSkillTool, SkillRegistry};
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::{EscapeRoot, ToolExecutor};
use entanglement_runtime::workdir::{self, SessionWorkdirs};

use super::ScriptedLlm;

/// How the test answers a `rhai` launch card.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    RunAll,
    AskPerCommand,
    Reject,
    /// Answer no card at all, launch or nested — nobody is there.
    Silent,
    /// `skutter run`'s answers (`pending::headless_starts` on the card's
    /// reason): an in-root launch card starts the script Ask per command,
    /// every other card is rejected.
    Headless,
}

/// The structured part of one card, launch or nested (ADR-0230).
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub tool: String,
    pub reason: Option<CardReason>,
    pub scopes: Vec<ApprovalScope>,
}

/// What one session saw.
pub struct Seen {
    /// Every `rhai` launch card's body.
    pub launch_cards: Vec<String>,
    /// Every other card — a nested call's (`<tool> (rhai)`) or a direct
    /// call's — as `<tool>: <body>`, approved once.
    pub nested_cards: Vec<String>,
    /// Every result of a call the model made, in order.
    pub outputs: Vec<String>,
    /// Every card's structured fields, in order.
    pub cards: Vec<Card>,
    /// The tool of every live `NestedCall` report (ADR-0233), in order.
    pub nested_calls: Vec<String>,
    /// The escape-root grant store the run recorded into.
    pub store: Arc<ExtraRootStore>,
}

/// The executor's variable inputs.
pub struct Setup {
    pub table: Arc<ModeTable>,
    pub launch: Launch,
    /// Wire the escape-root policy (as `main.rs` does).
    pub escape_root: bool,
    pub hooks: Hooks,
    /// Set on the session before its first prompt.
    pub overlay: Vec<ToolOverlayEntry>,
    /// The session's workdir, relative to the root.
    pub workdir: Option<&'static str>,
    /// `rhai.trusted` folders, canonical.
    pub trusted: TrustedFolders,
    pub gate: Arc<HttpGate>,
    /// The `config.yml` `permissions` ceiling.
    pub ceiling: PermissionProfile,
    /// The session's mode, set before its first prompt (default: the
    /// table's `build`).
    pub mode: Option<&'static str>,
    /// The scope every approval this harness sends carries.
    pub scope: ApprovalScope,
    /// The skill layer dirs the write floor protects.
    pub skill_dirs: Vec<PathBuf>,
    /// The live skill registry: `list_skills`, `load_skill` and the
    /// scripts' `skill::` namespaces all read it.
    pub skills: SkillRegistry,
}

impl Setup {
    pub fn new(table: Arc<ModeTable>, launch: Launch) -> Self {
        Setup {
            table,
            launch,
            escape_root: true,
            hooks: Hooks::default(),
            overlay: Vec::new(),
            workdir: None,
            trusted: TrustedFolders::default(),
            gate: Arc::default(),
            ceiling: PermissionProfile::new(Permission::Allow),
            mode: None,
            scope: ApprovalScope::Once,
            skill_dirs: Vec::new(),
            skills: SkillRegistry::default(),
        }
    }
}

/// A `build` mode grading every tool `default`, plus `allow`/`deny` rules,
/// unattended when `question_timeout` is nonzero.
pub fn table(
    default: Permission,
    allow: &[&str],
    deny: &[&str],
    question_timeout: u64,
) -> Arc<ModeTable> {
    let list = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Arc::new(
        ModeTable::new(vec![Mode {
            name: "build".into(),
            default,
            rules: Rules::from_lists(&list(deny), &list(allow), &[]),
            limits: Limits {
                question_timeout,
                ..Limits::default()
            },
            sandbox: None,
            sandbox_network: false,
        }])
        .unwrap(),
    )
}

/// A canonical project root `<tmp>/proj` with `scripts/add.rhai` and
/// `other.rhai`, and `<tmp>/evil.rhai` + `<tmp>/out.txt` outside it.
pub fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("proj");
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(root.join("scripts/add.rhai"), "args.a + args.b").unwrap();
    std::fs::write(root.join("other.rhai"), "args.a + args.b").unwrap();
    std::fs::write(base.join("evil.rhai"), "args.a + args.b").unwrap();
    std::fs::write(base.join("out.txt"), "outside text\n").unwrap();
    (tmp, root)
}

pub const ADD: &str = r#"{"path":"scripts/add.rhai","args":{"a":40,"b":2}}"#;

/// Run one session making each of `inputs` as a `rhai` call in turn.
pub async fn run(root: &Path, inputs: &[&str], setup: Setup) -> Seen {
    let calls: Vec<(&str, &str)> = inputs.iter().map(|i| (RHAI_TOOL, *i)).collect();
    run_calls(root, &calls, setup).await
}

/// Run one session making each `(tool, input)` call in turn.
pub async fn run_calls(root: &Path, calls: &[(&str, &str)], setup: Setup) -> Seen {
    let called: Vec<String> = calls.iter().map(|(t, _)| t.to_string()).collect();
    let mut scripted: Vec<LlmResponse> = calls
        .iter()
        .enumerate()
        .map(|(i, (tool, input))| LlmResponse {
            tool_calls: vec![ToolCall {
                id: format!("t{i}"),
                name: tool.to_string(),
                input: input.to_string(),
                provider_meta: None,
            }],
            ..Default::default()
        })
        .collect();
    scripted.push(LlmResponse {
        text: "ok".into(),
        ..Default::default()
    });
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let store = Arc::new(ExtraRootStore::ephemeral());
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm::new(scripted.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        workdir_resolver: Some(workdir::resolver(root.to_path_buf(), Some(store.clone()))),
        ..EngineConfig::default()
    });
    let workdirs = SessionWorkdirs::default();
    let skills = Arc::new(RwLock::new(Arc::new(setup.skills)));
    let mut tools = host_tools_with(
        root.to_path_buf(),
        HostToolOptions {
            extra_roots: setup.escape_root.then(|| store.clone()),
            backups: None,
            workdirs: Some(workdirs.clone()),
        },
    );
    tools.register(LoadSkillTool::new(skills.clone()));
    let tools = tools.shared();
    let base = setup.ceiling.clone();
    let modes = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        setup.table.clone(),
        tools.clone(),
        base.clone(),
        Some(root.to_path_buf()),
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load_from(None));
    let mut builder = ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .base(base)
        .perm_modes(modes)
        .resolver(resolver)
        .grants(grants)
        .hooks(setup.hooks)
        .workdirs(workdirs)
        .http_gate(setup.gate)
        .rhai_trusted(setup.trusted)
        .skill_dirs(setup.skill_dirs)
        .live_skills(skills)
        .mode_table(setup.table);
    if setup.escape_root {
        builder = builder.escape_root(EscapeRoot {
            root: root.to_path_buf(),
            store: store.clone(),
        });
    }
    let _executor = builder.spawn();
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    if let Some(mode) = setup.mode {
        holly
            .send(InMsg::SetMode {
                session: sid.clone(),
                mode: mode.into(),
            })
            .await
            .unwrap();
    }
    if !setup.overlay.is_empty() || setup.mode.is_some() {
        holly
            .send(InMsg::SetToolOverlay {
                session: sid.clone(),
                entries: setup.overlay,
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
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    } else {
        holly
            .send(InMsg::Spawn {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                agent: "general".into(),
                prompt: "go".into(),
                user: None,
                model: None,
                mode: None,
                workdir: setup
                    .workdir
                    .map(|w| root.join(w).to_string_lossy().into_owned()),
            })
            .await
            .unwrap();
    }
    let mut seen = Seen {
        launch_cards: Vec::new(),
        nested_cards: Vec::new(),
        outputs: Vec::new(),
        cards: Vec::new(),
        nested_calls: Vec::new(),
        store,
    };
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(10), sub.recv()).await {
        if ev.session() != Some(&sid) {
            continue;
        }
        match ev {
            OutEvent::ToolRequest {
                request_id,
                tool,
                input,
                reason,
                scopes,
                ..
            } => {
                let launch = tool == RHAI_TOOL;
                let headless = entanglement_runtime::pending::headless_starts(reason);
                seen.cards.push(Card {
                    tool: tool.clone(),
                    reason,
                    scopes,
                });
                let approve = |run_all| InMsg::Approve {
                    session: sid.clone(),
                    request_id: request_id.clone(),
                    scope: setup.scope,
                    mode: None,
                    run_all,
                };
                let reject = InMsg::Reject {
                    session: sid.clone(),
                    request_id: request_id.clone(),
                    reason: Some("no".into()),
                };
                let msg = if !launch {
                    seen.nested_cards.push(format!("{tool}: {input}"));
                    match setup.launch {
                        Launch::Silent => None,
                        Launch::Headless if headless => Some(approve(false)),
                        Launch::Headless => Some(reject),
                        _ => Some(approve(false)),
                    }
                } else {
                    seen.launch_cards.push(input);
                    match setup.launch {
                        Launch::RunAll => Some(approve(true)),
                        Launch::Headless if headless => Some(approve(false)),
                        Launch::AskPerCommand => Some(approve(false)),
                        Launch::Headless | Launch::Reject => Some(reject),
                        Launch::Silent => None,
                    }
                };
                if let Some(msg) = msg {
                    holly.send(msg).await.unwrap();
                }
            }
            OutEvent::ToolOutput { tool, output, .. } if called.contains(&tool) => {
                seen.outputs.push(output)
            }
            OutEvent::NestedCall { tool, .. } => seen.nested_calls.push(tool),
            OutEvent::Done { .. } => break,
            _ => {}
        }
    }
    seen
}
