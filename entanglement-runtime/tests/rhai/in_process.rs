//! ADR-0249 end to end, through the real executor and the production tool
//! surface: an embedder's in-process MCP server registered in each session's
//! scope, and per-session skills. Two sessions of two scope keys make the
//! same calls — the `mcp` envelope, `mcp_tools`, a script calling
//! `mcp::local::echo` and `skill::inv::who`, `list_skills`, `load_skill` —
//! and each sees its own tool list and skills, while the handler is told
//! which session (and, for a model call, which request) every call is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, LlmResponse, OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::mcp::{InProcessMcp, McpScope, McpScopes, McpServerConfig, McpToolDef};
use entanglement_runtime::skills::{LoadSkillTool, SkillMeta, SkillRegistry, SkillResolver};
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::AdvertisingState;
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::ToolExecutor;
use serde_json::{json, Value};

use super::{allow_all_table, collect_auto_approving, ScriptedLlm, TempDir};

/// `(session, request_id, tool, arguments)` of every call the handler got.
type Calls = Arc<Mutex<Vec<(String, String, String, Value)>>>;

/// Lists `echo` plus one tool named after the session's tenant prefix, and
/// echoes `msg`.
struct Recorder {
    calls: Calls,
}

#[async_trait]
impl InProcessMcp for Recorder {
    async fn list_tools(&self, session: &SessionId) -> Result<Vec<McpToolDef>> {
        let tenant = session
            .to_string()
            .split(':')
            .next()
            .unwrap_or("")
            .to_string();
        let def = |name: String| McpToolDef {
            name,
            description: "echo".into(),
            input_schema: json!({"type": "object",
                "properties": {"msg": {"type": "string"}}, "required": ["msg"]}),
        };
        Ok(vec![def("echo".into()), def(format!("only_{tenant}"))])
    }

    async fn call_tool(
        &self,
        session: &SessionId,
        request_id: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value> {
        let msg = arguments["msg"].as_str().unwrap_or("?").to_string();
        self.calls.lock().unwrap().push((
            session.to_string(),
            request_id.to_string(),
            name.to_string(),
            arguments,
        ));
        Ok(json!({"content": [{"type": "text", "text": format!("echo:{msg}")}]}))
    }
}

fn skills(description: &str, who: &str) -> Arc<SkillRegistry> {
    let mut reg = SkillRegistry::default();
    reg.insert(
        SkillMeta::new("inv", description, "Invoice steps.")
            .with_lib(&format!(r#"fn who() {{ "{who}" }}"#))
            .unwrap(),
    );
    Arc::new(reg)
}

fn call(id: &str, name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: input.to_string(),
        provider_meta: None,
    }
}

/// Every session's model makes these calls, one per round.
fn scripted() -> Vec<LlmResponse> {
    let calls = [
        call(
            "t0",
            "mcp",
            json!({"server": "local", "tool": "echo", "args": {"msg": "env"}}),
        ),
        call("t1", "mcp_tools", json!({"name": "local"})),
        call(
            "t2",
            RHAI_TOOL,
            json!({"script": r#"mcp::local::echo(#{msg: "script"}) + "|" + skill::inv::who()"#}),
        ),
        call("t3", "list_skills", json!({})),
        call("t4", "load_skill", json!({"skill_name": "inv"})),
    ];
    let mut out: Vec<LlmResponse> = calls
        .into_iter()
        .map(|c| LlmResponse {
            tool_calls: vec![c],
            ..Default::default()
        })
        .collect();
    out.push(LlmResponse {
        text: "ok".into(),
        ..Default::default()
    });
    out
}

/// One engine; `a:*` sessions get tenant a's skills, every other session the
/// live registry. Each session's scope key is its tenant prefix.
fn engine(dir: &TempDir, recorder: Arc<Recorder>) -> Holly {
    let tenant_a = skills("tenant a", "a");
    let resolver: SkillResolver =
        Arc::new(move |s: &SessionId| s.to_string().starts_with("a:").then(|| tenant_a.clone()));
    let live = Arc::new(RwLock::new(skills("global", "global")));
    let mut tools = host_tools(dir.path.clone());
    tools.register(LoadSkillTool::with_resolver(live.clone(), resolver.clone()));
    let tools = tools.shared();
    let handler: Arc<dyn InProcessMcp> = recorder;
    let scopes = McpScopes::new(
        Arc::new(move |s: &SessionId| {
            let mut local = McpServerConfig::from_in_process(handler.clone());
            local.capabilities = HashMap::from([("echo".to_string(), "read".to_string())]);
            Some(McpScope {
                key: s.to_string().split(':').next().unwrap_or("").to_string(),
                servers: HashMap::from([("local".to_string(), local)]),
                token_store: None,
            })
        }),
        entanglement_core::HttpClient::with_shared_state(
            entanglement_core::SharedStateLocation::Disabled,
            entanglement_core::RetryConfig::default(),
        )
        .unwrap(),
        Vec::new(),
    );
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let advertising = Arc::new(AdvertisingState::new());
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(ScriptedLlm::new(scripted())) as Box<dyn Llm>),
        agents: agents.clone(),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: tools.clone(),
            advertising: advertising.clone(),
            agent_specs: entanglement_runtime::subagent::agent_specs(),
        })),
        ..EngineConfig::default()
    });
    let _executor = ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .mode_table(allow_all_table())
        .live_skills(live)
        .skill_resolver(resolver)
        .mcp_scopes(scopes)
        .spawn();
    holly
}

/// Run one session to its end; each call's output by request id.
async fn run(holly: &Holly, sid: &SessionId) -> HashMap<String, String> {
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
    let events = tokio::time::timeout(
        Duration::from_secs(30),
        collect_auto_approving(holly, sub, sid),
    )
    .await
    .expect("the session finishes");
    events
        .into_iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput {
                request_id, output, ..
            } => Some((request_id, output)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn each_session_reaches_the_in_process_server_and_its_own_skills() {
    let dir = TempDir::new("in-process-mcp");
    let calls: Calls = Arc::default();
    let holly = engine(
        &dir,
        Arc::new(Recorder {
            calls: calls.clone(),
        }),
    );

    for (sid, tenant, skill) in [("a:1", "a", "tenant a"), ("b:1", "b", "global")] {
        let sid = SessionId::new(sid);
        let out = run(&holly, &sid).await;
        let get = |id: &str| {
            out.get(id)
                .cloned()
                .unwrap_or_else(|| panic!("{sid}: no {id}: {out:?}"))
        };
        assert_eq!(get("t0").trim(), "echo:env", "{sid}");
        let listed = get("t1");
        assert!(
            listed.contains(&format!("only_{tenant}")),
            "{sid}: {listed}"
        );
        let other = if tenant == "a" { "only_b" } else { "only_a" };
        assert!(
            !listed.contains(other),
            "{sid} sees another scope's list: {listed}"
        );
        let who = if tenant == "a" { "a" } else { "global" };
        assert!(
            get("t2").contains(&format!("echo:script|{who}")),
            "{sid}: {}",
            get("t2")
        );
        assert!(
            get("t3").contains(&format!("inv — {skill}")),
            "{sid}: {}",
            get("t3")
        );
        assert!(get("t4").contains("skill_id: inv"), "{sid}: {}", get("t4"));
    }

    let calls = calls.lock().unwrap().clone();
    let of = |sid: &str| -> Vec<_> { calls.iter().filter(|c| c.0 == sid).cloned().collect() };
    for sid in ["a:1", "b:1"] {
        let mine = of(sid);
        assert_eq!(mine.len(), 2, "{sid}: {calls:?}");
        assert_eq!(
            (mine[0].1.as_str(), mine[0].2.as_str()),
            ("t0", "echo"),
            "{sid}"
        );
        assert_eq!(mine[0].3["msg"], "env");
        assert_eq!(mine[1].2, "echo");
        assert_eq!(mine[1].3["msg"], "script");
        assert!(!mine[1].1.is_empty(), "a nested call carries a request id");
    }
}
