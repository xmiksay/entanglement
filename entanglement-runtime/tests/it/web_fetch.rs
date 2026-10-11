//! `web_fetch` (#736, ADR-0220) through the real dispatch path: the shipped
//! built-in mode table grades it (Exec — allowed in `build`, prompted in
//! `research`), and once cleared the SSRF guard refuses a loopback target as
//! an `is_error` tool result rather than reaching the socket. The guard's
//! own address matrix and the success/redirect/cap paths against live
//! fixture servers are unit-tested in `entanglement-provider`'s
//! `web_fetch::tests`, where a `cfg(test)`-only guard field can admit
//! `127.0.0.1` without any production bypass existing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::capability::Capability;
use entanglement_runtime::host::WebFetchTool;
use entanglement_runtime::mode::ModeTable;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_runner::grade_in_scope;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::{Tool, ToolRegistry};
use std::borrow::Cow;

use crate::mode_support::perm_modes;

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                ..Default::default()
            });
        Ok(stream_from_response(resp))
    }
}

/// One `tool` call to `url`, then a plain text reply.
fn script(tool: &str, url: &str) -> Vec<LlmResponse> {
    let call = LlmResponse {
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: tool.into(),
            input: serde_json::json!({ "url": url }).to_string(),
            provider_meta: None,
        }],
        ..Default::default()
    };
    // Popped from the back.
    vec![
        LlmResponse {
            text: "done".into(),
            ..Default::default()
        },
        call,
    ]
}

/// A `Read` tool that reports how the running call's in-scope grader
/// (#808) grades `web_fetch` of its `url` — proof the executor scopes one.
struct HopProbe;

#[async_trait]
impl Tool for HopProbe {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("hop_probe")
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Read]
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        let graded = grade_in_scope("web_fetch", input).await;
        Ok(format!("hop grade: {graded:?}"))
    }
}

/// Spawn an engine + executor over the real built-in mode table with
/// `web_fetch` and [`HopProbe`] registered, start session `s1` in `mode`,
/// have the model call `tool` on `url`, and collect events for `window`.
async fn run_in_mode(mode: &str, tool: &str, url: &str, window: Duration) -> Vec<OutEvent> {
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let scripted = script(tool, url);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: Mutex::new(scripted.clone()),
            }) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(WebFetchTool::new().expect("web_fetch client builds"));
    reg.register(HopProbe);
    let shared = reg.shared();
    let table = Arc::new(ModeTable::builtin().expect("built-in modes parse"));
    let modes = perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        table.clone(),
        shared.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load_from(None));
    let _executor = ToolExecutor::builder(&holly, shared)
        .agents(profiles)
        .base(PermissionProfile::new(Permission::Allow))
        .active(Arc::new(Mutex::new(HashMap::new())))
        .perm_modes(modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(table)
        .spawn();

    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: mode.into(),
        })
        .await
        .unwrap();
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if ev.session() == Some(&sid) {
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                break;
            }
        }
    }
    out
}

#[tokio::test]
async fn build_mode_runs_it_and_the_guard_refuses_a_loopback_target() {
    let events = run_in_mode(
        "build",
        "web_fetch",
        "http://127.0.0.1:9/",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "build allows web_fetch without a prompt: {events:?}"
    );
    let (output, is_error) = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("a tool output");
    assert!(is_error, "{output}");
    assert!(output.contains("refusing"), "{output}");
}

#[tokio::test]
async fn research_mode_prompts_before_any_fetch() {
    let events = run_in_mode(
        "research",
        "web_fetch",
        "https://example.com/",
        Duration::from_millis(1500),
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolRequest { tool, .. } if tool == "web_fetch"
        )),
        "research must park web_fetch on an approval: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { .. })),
        "nothing may run before the approval: {events:?}"
    );
}

fn first_output(events: &[OutEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a tool output")
}

/// #808: the executor scopes the call's own grading context around every
/// dispatched tool, so a redirect hop is graded by the session's real mode
/// — `research` asks for any fetch, `build` allows it.
#[tokio::test]
async fn the_executor_scopes_a_grader_matching_the_session_mode() {
    let research = run_in_mode(
        "research",
        "hop_probe",
        "https://crates.io/x",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        first_output(&research),
        "hop grade: Some((Ask, \"research\"))"
    );
    let build = run_in_mode(
        "build",
        "hop_probe",
        "https://crates.io/x",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(first_output(&build), "hop grade: Some((Allow, \"build\"))");
}
