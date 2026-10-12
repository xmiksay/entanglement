//! ADR-0226 runtime half: a `ToolExec` still named `mcp` reaches the runtime
//! only when core declined to unwrap it (malformed). It is declined with the
//! envelope's schema and never parks an approval; a stray retired `invoke` is
//! an ordinary unknown tool; an `mcp__<server>__<tool>` naming no registered
//! server says so. Core here has no `tool_spec_resolver`, so it never unwraps.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
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
            ..Default::default()
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

/// A session `s1` on `agent` whose scripted model makes one `name` call with
/// `input`.
async fn run_one_call(name: &str, input: &str, agent: &str) -> Vec<OutEvent> {
    let call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }],
        ..Default::default()
    };
    let ok = LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
        ..Default::default()
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
        agents: profiles.clone(),
        ..EngineConfig::default()
    });

    let sid = SessionId::new("s1");
    let mut reg = ToolRegistry::new();
    reg.register(EchoRead);
    let base = PermissionProfile::new(Permission::Allow);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let shared_tools = reg.shared();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        crate::mode_support::allow_all_table(),
        shared_tools.clone(),
        base.clone(),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load_from(None));
    let _executor = ToolExecutor::builder(&holly, shared_tools)
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
        .discovery(DiscoverySurface::default())
        .spawn();

    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: agent.into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
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
        "a declined call must never park an approval; got {events:?}"
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
async fn a_malformed_mcp_envelope_explains_the_schema() {
    for agent in ["general", "debug"] {
        let events = run_one_call("mcp", r#"{"srv":"gh","args":"nope"}"#, agent).await;
        let (output, is_error) = decline(&events);
        assert!(
            output.starts_with("malformed mcp call"),
            "{agent}: {output}"
        );
        assert!(output.contains(r#""required""#), "{agent}: {output}");
        assert!(is_error);
    }
}

#[tokio::test]
async fn a_stray_retired_invoke_is_an_ordinary_unknown_tool() {
    let events = run_one_call("invoke", r#"{"name":"read","args":{}}"#, "general").await;
    let (output, is_error) = decline(&events);
    assert!(output.starts_with("unknown tool: `invoke`"), "{output}");
    assert!(is_error);
}

#[tokio::test]
async fn an_mcp_call_to_an_unregistered_server_names_list_mcp() {
    let events = run_one_call("mcp__nope__search", "{}", "general").await;
    let (output, is_error) = decline(&events);
    assert!(
        output.starts_with("no MCP server named `nope` — list_mcp"),
        "{output}"
    );
    assert!(is_error);
}
