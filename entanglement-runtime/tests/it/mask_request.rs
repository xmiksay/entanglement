//! Out-of-mask tool calls as an approval round-trip (ADR-0198): the three
//! hard limits that still flat-decline with no prompt (a spawn tool, an
//! explicit bare-name `Deny` rule, an unknown name — the last already
//! covered by `tool_mask.rs`'s `mcp_enable_clears_the_mask_under_explore_and_research`
//! and `unregistered_bash_falls_through_to_the_generic_unknown_tool_message`),
//! and the approval scopes (`Once`/`Session`/`Reject`) for everything else.
//! `tool_mask.rs` covers the mask-attribution wording per authority; this
//! file covers the approval round-trip's own behavior.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentMode, AgentProfile, ApprovalScope, EngineConfig, Holly, InMsg, Llm,
    LlmRequest, LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ProfileRegistry,
    SessionId, ToolCall, ToolOverlayEntry,
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

/// A host tool that echoes its own name and input — registered under
/// whatever name a test needs (`edit`, an `mcp__*` name, ...).
struct EchoTool(&'static str);
#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran {}: {input}", self.0))
    }
}

fn call(id: &str, tool: &str) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: tool.into(),
            input: "{}".into(),
            provider_meta: None,
        }],
    }
}

fn text(s: &str) -> LlmResponse {
    LlmResponse {
        text: s.into(),
        tool_calls: vec![],
    }
}

fn profile(
    name: &str,
    tools: Option<Vec<&str>>,
    permission: PermissionProfile,
    can_spawn: Option<bool>,
) -> AgentProfile {
    AgentProfile {
        name: name.into(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: String::new(),
        model: None,
        provider: None,
        permission,
        tools: tools.map(|v| v.into_iter().map(String::from).collect()),
        disallowed_tools: Vec::new(),
        can_spawn,
        spawnable_agents: None,
        sandbox: None,
    }
}

/// Spawn a Holly scripted with `responses`, one profile named `agent.name`,
/// and `t` as the only registered host tool — session `s1`, already switched
/// onto `agent` and ready for a `prompt`.
async fn spawn(
    responses: Vec<LlmResponse>,
    agent: AgentProfile,
    t: EchoTool,
) -> (Holly, SessionId) {
    let scripted = Arc::new(responses);
    let mut profiles = ProfileRegistry::default();
    let agent_name = agent.name.clone();
    profiles.insert(agent);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(t);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: agent_name,
        })
        .await
        .unwrap();
    (holly, sid)
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

fn outputs(events: &[OutEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

fn requests_for<'a>(events: &'a [OutEvent], tool: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolRequest { tool: t, input, .. } if t == tool => Some(input.as_str()),
            _ => None,
        })
        .collect()
}

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

/// `build`-like: `edit` masked out, but the bare grade (default Allow, no
/// explicit rule) is not a hard-limit floor — the mask miss is the *only*
/// reason to ask.
fn allow_graded_masked_profile() -> AgentProfile {
    profile(
        "noedit-allow",
        Some(vec!["read"]),
        PermissionProfile::new(Permission::Allow),
        None,
    )
}

#[tokio::test]
async fn approve_once_runs_once_and_the_next_call_parks_again() {
    let (holly, sid) = spawn(
        vec![
            call("t1", "edit"),
            text("ok1"),
            call("t2", "edit"),
            text("ok2"),
        ],
        allow_graded_masked_profile(),
        EchoTool("edit"),
    )
    .await;
    let mut watch = holly.subscribe();
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit"))
        .await
        .unwrap();
    wait_for_request(&mut watch, "edit").await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Once,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        outputs(&events).iter().any(|o| o.starts_with("ran edit:")),
        "the approved call must run; got {events:?}"
    );

    // Second, unrelated turn: `Once` recorded nothing, so the identical call
    // parks all over again.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit again"))
        .await
        .unwrap();
    let input = wait_for_request(&mut watch, "edit").await;
    assert!(input.contains("outside agent profile `noedit-allow`'s tool mask"));
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id: "t2".into(),
            reason: None,
        })
        .await
        .unwrap();
    let events2 = collect(sub2, &sid).await;
    assert!(
        outputs(&events2)
            .iter()
            .any(|o| o.starts_with("tool `edit` rejected")),
        "the second call must park its own fresh approval; got {events2:?}"
    );
}

#[tokio::test]
async fn approve_session_runs_and_later_calls_pass_without_prompting() {
    let (holly, sid) = spawn(
        vec![
            call("t1", "edit"),
            text("ok1"),
            call("t2", "edit"),
            text("ok2"),
        ],
        allow_graded_masked_profile(),
        EchoTool("edit"),
    )
    .await;
    let mut watch = holly.subscribe();
    let mut overlay_watch = holly.subscribe();
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit"))
        .await
        .unwrap();
    wait_for_request(&mut watch, "edit").await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Session,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(outputs(&events).iter().any(|o| o.starts_with("ran edit:")));

    // The overlay state now carries a flat-allow enable entry for `edit`.
    let mut saw_entry = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), overlay_watch.recv()).await
    {
        if let OutEvent::ToolOverlayChanged { session, entries } = &ev {
            if session == &sid && entries.iter().any(|e| e.pattern == "edit" && e.allow) {
                saw_entry = true;
                break;
            }
        }
    }
    assert!(
        saw_entry,
        "Session approval must materialize an overlay enable entry"
    );

    // A later call to the same tool runs directly — no second ToolRequest.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit again"))
        .await
        .unwrap();
    let events2 = collect(sub2, &sid).await;
    assert!(
        requests_for(&events2, "edit").is_empty(),
        "a Session-approved tool must not prompt again; got {events2:?}"
    );
    assert!(outputs(&events2).iter().any(|o| o.starts_with("ran edit:")));
}

#[tokio::test]
async fn out_of_mask_and_allow_graded_is_a_single_prompt() {
    // The mask miss is the *only* reason to ask (bare grade defaults Allow,
    // no explicit rule for `edit`) — approving must run with no second
    // ToolRequest anywhere in the turn.
    let (holly, sid) = spawn(
        vec![call("t1", "edit"), text("ok")],
        allow_graded_masked_profile(),
        EchoTool("edit"),
    )
    .await;
    let mut watch = holly.subscribe();
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit"))
        .await
        .unwrap();
    wait_for_request(&mut watch, "edit").await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Once,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert_eq!(
        requests_for(&events, "edit").len(),
        1,
        "exactly one ToolRequest total; got {events:?}"
    );
    assert!(outputs(&events).iter().any(|o| o.starts_with("ran edit:")));
}

#[tokio::test]
async fn out_of_mask_and_ask_graded_may_legitimately_ask_twice() {
    // ADR-0198's documented trade-off: when the underlying grade is itself
    // `Ask` (not just the mask), a `Once` mask approval replays `dispatch`
    // unchanged, which legitimately parks its own `Ask` — the mask was not
    // the only reason to prompt.
    let ask_masked = profile(
        "noedit-ask",
        Some(vec!["read"]),
        PermissionProfile::new(Permission::Ask),
        None,
    );
    let (holly, sid) = spawn(
        vec![call("t1", "edit"), text("ok")],
        ask_masked,
        EchoTool("edit"),
    )
    .await;
    let mut watch = holly.subscribe();
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit"))
        .await
        .unwrap();
    let mask_input = wait_for_request(&mut watch, "edit").await;
    assert!(mask_input.contains("outside agent profile `noedit-ask`'s tool mask"));
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Once,
        })
        .await
        .unwrap();
    // The permission layer's own `Ask` now parks a second, ordinary
    // approval (no mask attribution this time) for the same request id.
    let second_input = wait_for_request(&mut watch, "edit").await;
    assert!(
        !second_input.contains("tool mask"),
        "the second prompt is the ordinary permission Ask, not another mask offer; got \
         {second_input:?}"
    );
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Once,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert_eq!(
        requests_for(&events, "edit").len(),
        2,
        "two total prompts; got {events:?}"
    );
    assert!(outputs(&events).iter().any(|o| o.starts_with("ran edit:")));
}

#[tokio::test]
async fn an_explicit_bare_deny_rule_still_flat_declines_no_prompt() {
    // The one hard-limit grade case: the profile author wrote `edit: deny`
    // explicitly (not just an ambient default) — a mask-miss approval offer
    // would be pointless, so it stays a flat decline exactly as before
    // ADR-0198.
    let denied = profile(
        "noedit-deny",
        Some(vec!["read"]),
        PermissionProfile::new(Permission::Allow).with("edit", Permission::Deny),
        None,
    );
    let (holly, sid) = spawn(
        vec![call("t1", "edit"), text("ok")],
        denied,
        EchoTool("edit"),
    )
    .await;
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        requests_for(&events, "edit").is_empty(),
        "an explicit Deny rule must never offer an approval; got {events:?}"
    );
    let outs = outputs(&events);
    assert!(
        outs.iter()
            .any(|o| o == "tool `edit` denied by permission profile"),
        "got {outs:?}"
    );
    assert!(!outs.iter().any(|o| o.starts_with("ran")));
}

#[tokio::test]
async fn spawn_tools_out_of_mask_still_flat_decline() {
    let no_spawn = profile(
        "noagent",
        Some(vec!["read"]),
        PermissionProfile::new(Permission::Allow),
        Some(true),
    );
    let (holly, sid) = spawn(
        vec![call("t1", "agent"), text("ok")],
        no_spawn,
        EchoTool("edit"),
    )
    .await;
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "spawn"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        requests_for(&events, "agent").is_empty(),
        "a masked spawn tool must never offer an approval; got {events:?}"
    );
    let outs = outputs(&events);
    assert!(
        outs.iter()
            .any(|o| o.contains("is not in its tool mask") && o.contains("`agent`")),
        "got {outs:?}"
    );
}

#[tokio::test]
async fn mcp_tool_out_of_mask_parks_like_any_other() {
    let masked = profile(
        "nomcp",
        Some(vec!["read"]),
        PermissionProfile::new(Permission::Allow),
        None,
    );
    let (holly, sid) = spawn(
        vec![call("t1", "mcp__chessbase__evaluate"), text("ok")],
        masked,
        EchoTool("mcp__chessbase__evaluate"),
    )
    .await;
    let mut watch = holly.subscribe();
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "evaluate"))
        .await
        .unwrap();
    let input = wait_for_request(&mut watch, "mcp__chessbase__evaluate").await;
    assert!(input.contains("outside agent profile `nomcp`'s tool mask"));
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Once,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        outputs(&events)
            .iter()
            .any(|o| o.starts_with("ran mcp__chessbase__evaluate:")),
        "got {events:?}"
    );
}

/// Sanity: the approval decision itself still rides the ordinary trusted
/// in-process `InMsg::Approve`/`Reject` frames (ADR-0069) — no new protocol
/// surface was added for ADR-0198, so every scenario above already exercises
/// the unchanged wire shape.
#[tokio::test]
async fn overlay_entry_written_by_a_session_approval_is_a_flat_allow_entry() {
    let (holly, sid) = spawn(
        vec![call("t1", "edit"), text("ok")],
        allow_graded_masked_profile(),
        EchoTool("edit"),
    )
    .await;
    let mut watch = holly.subscribe();
    let mut overlay_watch = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit"))
        .await
        .unwrap();
    wait_for_request(&mut watch, "edit").await;
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Session,
        })
        .await
        .unwrap();
    let entry = loop {
        match tokio::time::timeout(Duration::from_secs(2), overlay_watch.recv())
            .await
            .expect("timed out waiting for ToolOverlayChanged")
            .unwrap()
        {
            OutEvent::ToolOverlayChanged { session, entries } if session == sid => {
                if let Some(e) = entries.iter().find(|e| e.pattern == "edit") {
                    break e.clone();
                }
            }
            _ => {}
        }
    };
    assert_eq!(entry, ToolOverlayEntry::allow("edit"));
}
