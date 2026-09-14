//! Physical per-agent tool restriction — the **whole** enforcement (#116,
//! ADR-0038).
//!
//! Core advertises every schema it is given, so a masked tool's spec does reach
//! the model and the executor's dispatch gate is the only boundary: it refuses
//! the call *before* permission is resolved, the tool never runs, and the
//! refusal is **attributed** so the model learns who declined it instead of
//! retrying forever. Here the scripted LLM is forced to call `edit` under the
//! read-only `explore` profile (allowlist `read`/`glob`/`grep`), under an
//! overlay deny, and under a skill mask — asserting each authority's wording.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry, SessionId,
    ToolCall,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::{Tool, ToolRegistry};

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
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
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// A host tool named `edit` that records if it ever runs — the mask must stop it.
struct EchoEdit;
#[async_trait]
impl Tool for EchoEdit {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edit")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

/// Build a Holly whose scripted LLM calls `edit` once, wired with the runtime
/// executor over the built-in profiles (which include the masked `explore`).
fn spawn_with_edit_call() -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "edit".into(),
                input: "{\"path\":\"x\"}".into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve the `SetAgent { agent: "explore" }` below.
        profiles: entanglement_runtime::agents::built_in_registry()
            .expect("built-in agents must parse"),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoEdit);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        PermissionProfile::new(Permission::Allow),
    );
    holly
}

/// [`spawn_with_edit_call`] generalized: a scripted LLM that calls `tool` once,
/// over a caller-supplied profile registry, with only `EchoEdit` registered.
fn spawn_calling(tool: &str, profiles: ProfileRegistry) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: tool.to_string(),
                input: "{\"command\":\"true\"}".into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoEdit);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );
    holly
}

async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() == Some(sid) {
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                break;
            }
        }
    }
    out
}

/// Every `ToolOutput` text for `sid`.
fn outputs(events: &[OutEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

/// Whether any `ToolOutput` carried the ADR-0176 `is_error` flag.
fn any_is_error(events: &[OutEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, OutEvent::ToolOutput { is_error, .. } if *is_error))
}

#[tokio::test]
async fn masked_edit_is_declined_by_the_profile_and_never_runs() {
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    // Switch to the read-only `explore` profile: `edit` is outside its mask.
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "please edit"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a masked tool is declined outright, never surfaced for approval"
    );
    let outs = outputs(&events);
    assert!(
        outs.iter()
            .any(|o| o
                == "Declined by agent profile `explore` — tool `edit` is not in its tool mask"),
        "the decline must name the declining profile; got {outs:?}"
    );
    assert!(
        any_is_error(&events),
        "an autodecline rides the ADR-0176 side channel as an error; got {events:?}"
    );
    assert!(
        !outs.iter().any(|o| o.starts_with("ran:")),
        "the masked edit tool must never run"
    );
}

#[tokio::test]
async fn overlay_deny_is_attributed_to_the_overlay_not_the_profile() {
    // #539/ADR-0149's deny half is dispatch-only now. `build` advertises and
    // permits `edit`; a per-session deny withdraws it — and must say so, since
    // blaming the agent definition would send the user editing a file that is
    // not the cause.
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries: vec![entanglement_core::ToolOverlayEntry::deny("edit")],
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "please edit"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    let outs = outputs(&events);
    assert!(
        outs.iter()
            .any(|o| o
                == "Declined by session tool overlay — tool `edit` is withdrawn for this session"),
        "an overlay deny must be attributed to the overlay; got {outs:?}"
    );
    assert!(
        !outs.iter().any(|o| o.starts_with("ran:")),
        "the denied edit tool must never run"
    );
}

#[tokio::test]
async fn build_profile_runs_edit_unmasked() {
    // Control: the default `build` profile has no mask, so `edit` runs normally.
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit it"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "unmasked build should run edit; got {events:?}"
    );
}

#[tokio::test]
async fn an_ancestors_mask_declines_a_child_and_names_the_ancestor() {
    // ADR-0038's ancestor-chain intersection is fully capability-enforcing at
    // dispatch: a read-only parent's sub-tree can never reach write capability,
    // however permissive the child's own definition is. #597: the refusal names
    // the *ancestor*, since a child whose own mask lists `edit` would otherwise
    // read as an inexplicable dead end.
    let mut profiles = ProfileRegistry::default();
    profiles.insert(AgentProfile {
        name: "restricted".into(),
        description: "read-only parent".into(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Allow),
        tools: Some(vec!["read".into(), "agent".into()]),
        disallowed_tools: Vec::new(),
        can_spawn: Some(true),
        spawnable_agents: Some(vec!["worker".into()]),
        sandbox: None,
    });
    profiles.insert(AgentProfile {
        name: "worker".into(),
        description: "permissive child".into(),
        mode: AgentMode::Subagent,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission: PermissionProfile::new(Permission::Allow),
        // The child's own mask happily lists `edit` — only the parent's doesn't.
        tools: Some(vec!["read".into(), "edit".into()]),
        disallowed_tools: Vec::new(),
        can_spawn: Some(false),
        spawnable_agents: None,
        sandbox: None,
    });
    let holly = spawn_calling("edit", profiles);
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    holly
        .send(InMsg::SetAgent {
            session: parent.clone(),
            agent: "restricted".into(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::prompt(parent.clone(), "start"))
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "worker".into(),
            prompt: "edit something".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();
    let events = collect(sub, &child).await;
    let outs = outputs(&events);
    assert!(
        outs.iter().any(|o| o
            == "Declined by ancestor agent `restricted`'s profile — tool `edit` is not in its \
                tool mask"),
        "the child's decline must name the clamping ancestor; got {outs:?}"
    );
    assert!(
        !outs.iter().any(|o| o.starts_with("ran:")),
        "the ancestor-masked edit must never run"
    );
}

/// The explore/research provider-bundled-MCP fix: `mcp_enable` must clear
/// the tool mask under both least-privileged profiles — pinned the same way
/// `unregistered_bash_falls_through_to_the_generic_unknown_tool_message`
/// below pins an admitted-but-unregistered name: if the mask still declined
/// it, dispatch would never even reach the registry lookup, so the specific
/// wording here (an ordinary "unknown tool", not "Declined by agent
/// profile") is itself the assertion that the mask let the call through.
#[tokio::test]
async fn mcp_enable_clears_the_mask_under_explore_and_research() {
    for agent in ["explore", "research"] {
        let holly = spawn_calling(
            "mcp_enable",
            entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        );
        let sid = SessionId::new("s1");
        holly
            .send(InMsg::SetAgent {
                session: sid.clone(),
                agent: agent.into(),
            })
            .await
            .unwrap();
        let sub = holly.subscribe();
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
        let events = collect(sub, &sid).await;
        let outs = outputs(&events);
        assert!(
            outs.iter()
                .any(|o| o.starts_with("unknown tool: `mcp_enable`")),
            "{agent}: mcp_enable must clear the mask (unregistered in this test registry, \
             so it falls through to the ordinary unknown-tool message); got {outs:?}"
        );
        assert!(
            !outs.iter().any(|o| o.contains("is not in its tool mask")),
            "{agent}: mcp_enable must not be mask-declined; got {outs:?}"
        );
    }
}

#[tokio::test]
async fn unregistered_bash_falls_through_to_the_generic_unknown_tool_message() {
    // ADR-0195 retired the lazily-registrable built-in machinery: `bash` is
    // registered at startup like every other tool, so an *unregistered*
    // `bash` is now possible only in a bespoke test registry like this one —
    // and it must behave like any other unknown name (the Levenshtein-hint
    // message), not carry a bespoke "enable with /enable tool bash" decline.
    let holly = spawn_calling(
        "bash",
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "run it"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    let outs = outputs(&events);
    assert!(
        outs.iter().any(|o| o.starts_with("unknown tool: `bash`")),
        "a name absent from the registry is an ordinary unknown tool; got {outs:?}"
    );
    assert!(
        !outs.iter().any(|o| o.contains("enable with /enable tool")),
        "the retired lazy-builtin decline must not fire; got {outs:?}"
    );
    assert!(
        any_is_error(&events),
        "the decline rides the ADR-0176 side channel as an error; got {events:?}"
    );
}
