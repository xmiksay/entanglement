//! Permission-mode dispatch grading (ADR-0207 stage 4), covering the ground
//! this file used to cover as the physical per-agent tool mask (#116,
//! ADR-0038) before ADR-0207 retired it ("the mask machinery is deleted").
//!
//! Grading now comes entirely from the session's permission **mode**
//! (`crate::mode::Mode`, resolved by `crate::policy::ProfileResolver`), never
//! from `AgentProfile`. A mode `deny` is **absolute**: a flat decline with no
//! prompt, naming the mode and the way out — there is no approval-offer
//! softening left to test (ADR-0198, which this ADR supersedes). The
//! remaining coverage here: a class-denied capability flat-declines, an
//! unmasked mode runs its tool normally, the ancestor-chain privilege clamp
//! (kept unchanged by ADR-0207 stage 4) still clamps a child spawned under a
//! more permissive mode down to its restrictive parent's grade, and an
//! unregistered name still falls through to the ordinary unknown-tool reply
//! regardless of mode.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, ProfileRegistry, SessionId, ToolCall,
    ToolOverlayEntry,
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

/// A host tool named `edit` that records if it ever runs. Declares
/// `Capability::Write` explicitly (it happens to match `Tool::capabilities`'s
/// fail-closed default, but this file's whole point is mode-capability
/// grading, so the tests should not lean on an implicit default).
struct EchoEdit;
#[async_trait]
impl Tool for EchoEdit {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edit")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [entanglement_runtime::capability::Capability] {
        &[entanglement_runtime::capability::Capability::Write]
    }
}

/// Build a Holly whose scripted LLM calls `edit` once, wired with the runtime
/// executor over the built-in profiles + built-in modes (`spawn_tool_executor`
/// defaults to `ModeTable::builtin()`).
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
async fn write_capability_class_denied_under_research_mode_declines_flat() {
    // ADR-0207 §4: `research` mode class-denies `write`, and a mode `deny` is
    // absolute — no approval offer, unlike the retired ADR-0198 mask-miss
    // softening. The message names the mode and the way out.
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "research".into(),
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
        "a mode deny never parks an approval; got {events:?}"
    );
    let outs = outputs(&events);
    assert!(
        outs.iter()
            .any(|o| o.contains("denied by mode `research`") && o.contains("/mode")),
        "the denial must name the mode and the way out; got {outs:?}"
    );
    assert!(any_is_error(&events), "got {events:?}");
    assert!(
        !outs.iter().any(|o| o.starts_with("ran:")),
        "a denied call must never run"
    );
}

#[tokio::test]
async fn build_mode_runs_edit_unmasked() {
    // Control: `build` mode class-allows `write`, so `edit` runs normally —
    // no mask, no prompt.
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
        "build mode should run edit; got {events:?}"
    );
}

#[tokio::test]
async fn an_ancestors_restrictive_mode_clamps_a_childs_more_permissive_default() {
    // The ancestor-chain privilege clamp (ADR-0024) is kept unchanged by
    // ADR-0207 stage 4: it still mins the grade across a session and its
    // ancestors, each resolved from its own mode. A spawned child always
    // starts in `DEFAULT_MODE` ("build", permissive) — mode inheritance down
    // the spawn tree is a later stage's wiring — but a `research`-mode
    // parent's class-deny on `write` still clamps the child's `edit` call
    // down to a flat decline via this same chain-min, exactly as it would
    // have for any other grade.
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = spawn_calling("edit", profiles);
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    holly
        .send(InMsg::SetMode {
            session: parent.clone(),
            mode: "research".into(),
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
            agent: "explore".into(),
            prompt: "edit something".into(),
            user: None,
            sponsored: false,
        })
        .await
        .unwrap();

    let events = collect(sub, &child).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "the ancestor clamp denies flat, no prompt; got {events:?}"
    );
    let outs = outputs(&events);
    assert!(
        outs.iter().any(|o| o.contains("denied by mode")),
        "the child's own build-mode default must be clamped down by the \
         research-mode parent; got {outs:?}"
    );
}

/// A name absent from the registry is an ordinary unknown tool under every
/// mode — nothing intercepts it earlier now that the mask is gone.
#[tokio::test]
async fn unregistered_bash_falls_through_to_the_generic_unknown_tool_message() {
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

/// An unregistered `mcp_enable` still falls through to the ordinary
/// unknown-tool reply under every one of the four built-in modes — a
/// regression pin that no mode-grading path accidentally intercepts it
/// earlier (the way the retired mask used to have its own carve-out here).
#[tokio::test]
async fn mcp_enable_falls_through_to_unknown_tool_under_every_built_in_mode() {
    for mode in ["research", "plan", "build", "auto"] {
        let holly = spawn_calling(
            "mcp_enable",
            entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        );
        let sid = SessionId::new("s1");
        holly
            .send(InMsg::SetMode {
                session: sid.clone(),
                mode: mode.into(),
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
            "{mode}: mcp_enable must fall through to the unknown-tool message \
             (unregistered in this test registry); got {outs:?}"
        );
    }
}

/// #560, ADR-0207 §3 (gap 1 of stage 4b): a `Capability::Control` tool is
/// never graded. `update_tasks` carries no registry entry (#231, ADR-0049;
/// exempted from the unknown-tool check by `is_state_tool`) and must run
/// unprompted under every built-in mode, including `auto`'s unattended
/// `default: deny`. Before this fix it fell through to the generic ladder
/// and prompted under `build`'s `default: prompt`.
#[tokio::test]
async fn update_tasks_runs_unprompted_under_every_built_in_mode() {
    for mode in ["research", "plan", "build", "auto"] {
        let holly = spawn_calling(
            "update_tasks",
            entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse"),
        );
        let sid = SessionId::new("s1");
        holly
            .send(InMsg::SetMode {
                session: sid.clone(),
                mode: mode.into(),
            })
            .await
            .unwrap();
        let sub = holly.subscribe();
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
        let events = collect(sub, &sid).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
            "{mode}: a Control tool must never park an approval; got {events:?}"
        );
        let outs = outputs(&events);
        assert!(
            outs.iter().any(|o| o == "tasks updated"),
            "{mode}: update_tasks must run and ack; got {outs:?}"
        );
    }
}

/// #634 (gap 2 of stage 4b): a matching overlay **deny** entry — inert since
/// ADR-0207 stage 4a deleted `tool_mask_source` — must decline the call flat
/// again, checked before the mode grade even runs. `build` mode alone would
/// allow `edit` outright, so any decline here is attributable to the overlay.
#[tokio::test]
async fn overlay_deny_declines_the_call_flat() {
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries: vec![ToolOverlayEntry::deny("edit")],
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
    holly
        .send(InMsg::prompt(sid.clone(), "edit it"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an overlay deny declines flat, no prompt; got {events:?}"
    );
    let outs = outputs(&events);
    assert!(
        outs.iter()
            .any(|o| o.contains("withdrawn for this session") && o.contains("/enable")),
        "the decline must name the withdrawal and the way out; got {outs:?}"
    );
    assert!(any_is_error(&events), "got {events:?}");
    assert!(
        !outs.iter().any(|o| o.starts_with("ran:")),
        "a denied call must never run"
    );
}

/// ADR-0207 §8: an overlay **enable** still overrides a mode `deny` — the
/// user's own `/enable tool X --allow` is trusted-frame-only (ADR-0177), so
/// only the user (never the model) can reach it. Regression pin alongside
/// the deny restore above so neither posture regresses while fixing the
/// other. `research` mode class-denies `write`, so `edit` running here can
/// only be the overlay.
#[tokio::test]
async fn overlay_enable_beats_a_mode_deny() {
    let holly = spawn_with_edit_call();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "research".into(),
        })
        .await
        .unwrap();
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries: vec![ToolOverlayEntry::allow("edit")],
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
    holly
        .send(InMsg::prompt(sid.clone(), "edit it"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    let outs = outputs(&events);
    assert!(
        outs.iter().any(|o| o.starts_with("ran:")),
        "an overlay enable must override the mode's write deny; got {outs:?}"
    );
}
