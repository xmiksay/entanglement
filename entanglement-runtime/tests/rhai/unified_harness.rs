//! The shared harness of the ADR-0227 unified-binding tests: one session
//! whose scripted model calls `rhai`, against a configurable executor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmResponse, OutEvent, Permission, PermissionProfile,
    SessionId, ToolCall,
};
use entanglement_runtime::hooks::Hooks;
use entanglement_runtime::host::{host_tools, BashTool};
use entanglement_runtime::http_gate::HttpGate;
use entanglement_runtime::mcp::AvailableMcp;
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};

use super::{collect_auto_approving, ScriptedLlm};

/// A `grep` that outlasts any script budget, signalling `entered` as it
/// starts.
struct SlowGrep {
    entered: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl entanglement_runtime::Tool for SlowGrep {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("grep")
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        self.entered.notify_one();
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok("too late".into())
    }
}

/// The executor's variable inputs for one scripted `rhai` run.
pub(super) struct Setup {
    pub(super) mode: Mode,
    pub(super) hooks: Hooks,
    pub(super) gate: Arc<HttpGate>,
    pub(super) avail: AvailableMcp,
    /// Register the host `bash` tool too.
    pub(super) bash: bool,
    /// Replace `grep` with one that takes 30 s, to trip the bridge clock.
    pub(super) slow_grep: bool,
    /// Notified as the slow `grep` starts running.
    pub(super) slow_grep_entered: Arc<tokio::sync::Notify>,
    pub(super) timeout: u64,
    /// Model turns scripted after the `rhai` call, before the closing text.
    pub(super) follow_up: Vec<LlmResponse>,
    /// Run the script detached (`background: true`, #637).
    pub(super) background: bool,
    /// A persistence tap to attach before the session starts.
    pub(super) sink: Option<Arc<dyn entanglement_runtime::persistence::RecordSink>>,
}

impl Setup {
    pub(super) fn mode(default: Permission, allow: &[&str], question_timeout: u64) -> Self {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        Setup {
            mode: Mode {
                name: "build".into(),
                default,
                rules: Rules::from_lists(&[], &allow, &[]),
                limits: Limits {
                    question_timeout,
                    ..Limits::default()
                },
                sandbox: None,
                sandbox_network: false,
            },
            hooks: Hooks::default(),
            gate: Arc::default(),
            avail: AvailableMcp::default(),
            bash: false,
            slow_grep: false,
            slow_grep_entered: Arc::default(),
            timeout: 20,
            follow_up: vec![],
            background: false,
            sink: None,
        }
    }
}

/// A session `s1` whose model calls `rhai` once with `script`; returns its
/// events with every approval auto-granted.
pub(super) async fn run(script: &str, root: &std::path::Path, setup: Setup) -> Vec<OutEvent> {
    let (holly, sub, sid) = start(script, root, setup).await;
    collect_auto_approving(&holly, sub, &sid).await
}

/// Spawn [`run`]'s session and hand back its handle, a subscription taken
/// before the spawn, and its id — for a caller collecting on its own terms.
pub(super) async fn start(
    script: &str,
    root: &std::path::Path,
    setup: Setup,
) -> (Holly, tokio::sync::broadcast::Receiver<OutEvent>, SessionId) {
    let mut input = serde_json::json!({ "script": script, "timeout": setup.timeout });
    if setup.background {
        input["background"] = true.into();
    }
    let input = input.to_string();
    let mut scripted = vec![LlmResponse {
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: RHAI_TOOL.into(),
            input,
            provider_meta: None,
        }],
        ..Default::default()
    }];
    scripted.extend(setup.follow_up);
    scripted.push(LlmResponse {
        text: "ok".into(),
        ..Default::default()
    });
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(ScriptedLlm::new(scripted.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    let mut tools = host_tools(root.to_path_buf());
    if setup.bash {
        tools.register(BashTool::new(root.to_path_buf()));
    }
    if setup.slow_grep {
        tools.register(SlowGrep {
            entered: setup.slow_grep_entered.clone(),
        });
    }
    let table = Arc::new(ModeTable::new(vec![setup.mode]).unwrap());
    let _executor = ToolExecutor::builder(&holly, tools.shared())
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .mode_table(table)
        .hooks(setup.hooks)
        .http_gate(setup.gate)
        .discovery(DiscoverySurface {
            mcp_avail: Arc::new(setup.avail),
            mcp_active: Arc::new(Mutex::new(HashMap::new())),
            http: Some(
                entanglement_core::HttpClient::with_shared_state(
                    entanglement_core::SharedStateLocation::Disabled,
                    entanglement_core::RetryConfig::default(),
                )
                .unwrap(),
            ),
            ..Default::default()
        })
        .spawn();
    if let Some(sink) = setup.sink {
        entanglement_runtime::persistence::spawn_persistence_subscriber_with_sink(&holly, sink);
    }
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
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
            workdir: None,
        })
        .await
        .unwrap();
    (holly, sub, sid)
}
