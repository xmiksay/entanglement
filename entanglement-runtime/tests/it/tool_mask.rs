//! Physical per-agent tool restriction — the **whole** enforcement (#116,
//! ADR-0038), and its ADR-0198 softening.
//!
//! Core advertises every schema it is given, so a masked tool's spec does reach
//! the model. Since ADR-0198, the executor's dispatch gate parks a mask-
//! attributed approval for most mask misses instead of declining outright —
//! the **attribution** (which link, on whose authority: profile mask or
//! session overlay) carries into the approval offer's text exactly as it did
//! into the old flat decline, so the model (or the user reviewing the
//! prompt) still learns *who* withheld the tool. Here the scripted LLM is
//! forced to call `edit` under the read-only `explore` profile (allowlist
//! `read`/`glob`/`grep`), under an overlay deny, and under an ancestor's
//! mask — asserting each authority's wording; the remaining hard-limit and
//! full approval-scope coverage (Once/Session/Reject, the explicit-Deny
//! floor, spawn tools, MCP) lives in `mask_request.rs`.

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

/// Wait for a `ToolRequest` naming `tool`, returning its `input` text (ADR-0198's
/// mask-attributed approvals append their attribution here — there is no
/// separate reason field). Panics if none arrives within the timeout, since a
/// caller reaching for this helper expects the call to have parked, not
/// declined outright.
async fn wait_for_request(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    tool: &str,
) -> String {
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::ToolRequest { tool: t, input, .. } = &ev {
            if t == tool {
                return input.clone();
            }
        }
    }
    panic!("expected `{tool}` to park a ToolRequest, none arrived");
}

#[tokio::test]
async fn masked_edit_under_explore_parks_an_approval_instead_of_declining() {
    // ADR-0198: `edit` is outside `explore`'s mask, but `explore`'s
    // permission rules never explicitly name `edit` — only the ambient
    // `default: deny` reaches it, which is not a hard-limit floor (#560's
    // `explicit_bare_deny`) — so this is no longer a flat decline. It parks
    // a mask-attributed approval; rejecting it declines as an error, exactly
    // as any other rejected approval would.
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "please edit"))
        .await
        .unwrap();

    let input = wait_for_request(&mut watch, "edit").await;
    assert!(
        input.contains("outside agent profile `explore`'s tool mask"),
        "the offer must name the declining profile; got {input:?}"
    );

    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id: "t1".into(),
            reason: None,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    let outs = outputs(&events);
    assert!(
        outs.iter().any(|o| o.starts_with("tool `edit` rejected")),
        "a rejected mask approval declines like any other; got {outs:?}"
    );
    assert!(any_is_error(&events), "got {events:?}");
    assert!(
        !outs.iter().any(|o| o.starts_with("ran:")),
        "a rejected mask approval must never run the tool"
    );
}

#[tokio::test]
async fn overlay_deny_parks_an_approval_attributed_to_the_overlay() {
    // #539/ADR-0149's deny half is dispatch-only. `build` advertises and
    // permits `edit`; a per-session deny withdraws it. Since ADR-0198 that
    // withdrawal is a mask miss like any other: it parks, attributed to the
    // overlay rather than the (unrelated) agent definition.
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
    let mut watch = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "please edit"))
        .await
        .unwrap();

    let input = wait_for_request(&mut watch, "edit").await;
    assert!(
        input.contains("withdrawn by this session's tool overlay"),
        "an overlay deny must be attributed to the overlay; got {input:?}"
    );

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: entanglement_core::ApprovalScope::Once,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    let outs = outputs(&events);
    assert!(
        outs.iter().any(|o| o.starts_with("ran:")),
        "an approved mask offer must run the tool; got {outs:?}"
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
async fn an_ancestors_mask_parks_a_child_approval_naming_the_ancestor() {
    // ADR-0038's ancestor-chain intersection still gates existence at
    // dispatch: a read-only parent's sub-tree can't reach write capability
    // unasked, however permissive the child's own definition is. Since
    // ADR-0198 that gate is soft — it parks an approval rather than
    // declining outright — but #597's attribution still applies: the offer
    // names the *ancestor*, since a child whose own mask lists `edit` would
    // otherwise read as an inexplicable dead end.
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
    let mut watch = holly.subscribe();
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

    // ADR-0198: the ancestor's mask miss now parks, attributed to the
    // clamping ancestor rather than an outright decline — `restricted`'s
    // permission rules never explicitly name `edit`, so it is not the
    // hard-limit floor either.
    let input = loop {
        match tokio::time::timeout(Duration::from_secs(2), watch.recv())
            .await
            .expect("timed out waiting for the child's mask offer")
            .unwrap()
        {
            OutEvent::ToolRequest {
                session,
                tool,
                input,
                ..
            } if session == child && tool == "edit" => break input,
            _ => {}
        }
    };
    assert!(
        input.contains("outside ancestor agent `restricted`'s profile tool mask"),
        "the offer must name the clamping ancestor; got {input:?}"
    );

    holly
        .send(InMsg::Approve {
            session: child.clone(),
            request_id: "t1".into(),
            scope: entanglement_core::ApprovalScope::Once,
        })
        .await
        .unwrap();
    let events = collect(sub, &child).await;
    let outs = outputs(&events);
    assert!(
        outs.iter().any(|o| o.starts_with("ran:")),
        "approving the ancestor-masked offer must run the tool; got {outs:?}"
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
