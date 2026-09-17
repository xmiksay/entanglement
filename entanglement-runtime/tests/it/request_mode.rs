//! Integration tests for the runtime-owned `request_mode` tool (#560,
//! ADR-0207 §10): a blocked model's escape hatch out of a mode that denies
//! it a needed tool.
//!
//! `Capability::Control`, so it is never graded by the session's permission
//! mode — but it still force-parks an approval, exactly like `propose_plan`,
//! for every request that clears its four refusal checks (malformed input,
//! current mode `auto`, target `auto`, non-widening pair). Uses the real
//! built-in `ModeTable` (`research`/`plan`/`build`/`auto`) throughout, since
//! the widen/narrow/auto rules are about mode *names*, not grading.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::mode::ModeTable;
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, PermissionResolver, ProfileResolver,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_names::REQUEST_MODE_TOOL;
use entanglement_runtime::tool_runner::spawn_tool_executor_with_policy;
use entanglement_runtime::ToolRegistry;

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

fn request_mode_call(id: &str, mode: &str) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: REQUEST_MODE_TOOL.into(),
            input: serde_json::json!({"mode": mode, "reason": "need it"}).to_string(),
            provider_meta: None,
        }],
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.into(),
        tool_calls: vec![],
    }
}

/// A trivial no-op registry is enough — `request_mode` touches no host
/// resource, so this harness never registers a single `Tool`.
fn spawn_with_builtin_modes(llm_factory: Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>) -> Holly {
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory,
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let reg = ToolRegistry::new();
    let shared_tools = reg.shared();
    let active = Arc::new(Mutex::new(HashMap::new()));
    let perm_modes: Arc<Mutex<HashMap<SessionId, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ProfileResolver::new(
        perm_modes.clone(),
        Arc::new(ModeTable::builtin().expect("built-in modes must parse")),
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
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
        None,
        None,
    );
    holly
}

async fn set_mode_and_wait(holly: &Holly, sid: &SessionId, mode: &str) {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: mode.to_string(),
        })
        .await
        .unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::ModeChanged {
            session, mode: got, ..
        } = &ev
        {
            if session == sid && got == mode {
                return;
            }
        }
    }
    panic!("timed out waiting for ModeChanged({mode})");
}

async fn collect_until_done(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    timeout: Duration,
) -> Vec<OutEvent> {
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(timeout, sub.recv()).await {
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == sid);
        if ev.session() == Some(sid) {
            events.push(ev);
        }
        if done {
            break;
        }
    }
    events
}

/// Set `sid`'s mode, then run the turn whose `request_mode` call is already
/// baked into `holly`'s scripted `Llm` factory, collecting every event
/// through to `Done`.
async fn run_one_call(holly: &Holly, sid: &SessionId, current_mode: &str) -> Vec<OutEvent> {
    set_mode_and_wait(holly, sid, current_mode).await;
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    collect_until_done(&mut sub, sid, Duration::from_secs(3)).await
}

fn is_error_output(events: &[OutEvent], contains: &str) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            OutEvent::ToolOutput { tool, output, is_error, .. }
                if tool == REQUEST_MODE_TOOL && *is_error && output.contains(contains)
        )
    })
}

fn has_tool_request(events: &[OutEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, OutEvent::ToolRequest { tool, .. } if tool == REQUEST_MODE_TOOL))
}

// --- 1. Widening requests force-park -----------------------------------

#[tokio::test]
async fn research_to_plan_widens_and_parks() {
    let scripted = Arc::new(vec![request_mode_call("r1", "plan"), text_response("ok")]);
    let holly = spawn_with_builtin_modes(Arc::new(move || {
        Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
    }));
    let sid = SessionId::new("s1");
    let events = run_one_call(&holly, &sid, "research").await;
    assert!(
        has_tool_request(&events),
        "research -> plan widens; must force-park: {events:?}"
    );
}

#[tokio::test]
async fn research_to_build_widens_and_parks() {
    let scripted = Arc::new(vec![request_mode_call("r1", "build"), text_response("ok")]);
    let holly = spawn_with_builtin_modes(Arc::new(move || {
        Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
    }));
    let sid = SessionId::new("s1");
    let events = run_one_call(&holly, &sid, "research").await;
    assert!(
        has_tool_request(&events),
        "research -> build widens; must force-park: {events:?}"
    );
}

#[tokio::test]
async fn plan_to_build_widens_and_parks() {
    let scripted = Arc::new(vec![request_mode_call("r1", "build"), text_response("ok")]);
    let holly = spawn_with_builtin_modes(Arc::new(move || {
        Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
    }));
    let sid = SessionId::new("s1");
    let events = run_one_call(&holly, &sid, "plan").await;
    assert!(
        has_tool_request(&events),
        "plan -> build widens; must force-park: {events:?}"
    );
}

// --- 2. Approval applies the switch, rejection folds the reason --------

#[tokio::test]
async fn approval_switches_the_session_mode() {
    let scripted = Arc::new(vec![
        request_mode_call("r1", "build"),
        text_response("continuing"),
    ]);
    let holly = spawn_with_builtin_modes(Arc::new(move || {
        Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
    }));
    let sid = SessionId::new("s1");
    set_mode_and_wait(&holly, &sid, "research").await;
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut request_id = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::ToolRequest {
            request_id: rid,
            tool,
            ..
        } = &ev
        {
            if tool == REQUEST_MODE_TOOL {
                request_id = Some(rid.clone());
                break;
            }
        }
    }
    let request_id = request_id.expect("request_mode must force-park");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    // Deferred while the turn is live, exactly like `SetAgent`
    // (session.rs's stash) — `run_request_mode` sends `SetMode` *before* the
    // `ToolResult` that continues the turn, so `ModeChanged` lands only once
    // the whole turn concludes, not synchronously with the `ToolOutput`.
    // `collect_until_done` stops right at `Done`, so collect a little past it
    // here instead.
    let mut events = Vec::new();
    let mut saw_done = false;
    loop {
        let per_event_timeout = if saw_done {
            Duration::from_millis(300)
        } else {
            Duration::from_secs(3)
        };
        let Ok(Ok(ev)) = tokio::time::timeout(per_event_timeout, sub.recv()).await else {
            break;
        };
        if matches!(&ev, OutEvent::Done { session, .. } if session == &sid) {
            saw_done = true;
        }
        if ev.session() == Some(&sid) {
            events.push(ev);
        }
    }
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ModeChanged { session, mode } if session == &sid && mode == "build"
        )),
        "approval must switch the session's mode to the requested target: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, is_error, .. }
                if tool == REQUEST_MODE_TOOL && !is_error
        )),
        "approval must reply with a non-error result: {events:?}"
    );
}

#[tokio::test]
async fn rejection_folds_the_reason_back_and_leaves_the_mode_unchanged() {
    let scripted = Arc::new(vec![request_mode_call("r1", "build"), text_response("ok")]);
    let holly = spawn_with_builtin_modes(Arc::new(move || {
        Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
    }));
    let sid = SessionId::new("s1");
    set_mode_and_wait(&holly, &sid, "research").await;
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut request_id = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
        if let OutEvent::ToolRequest {
            request_id: rid,
            tool,
            ..
        } = &ev
        {
            if tool == REQUEST_MODE_TOOL {
                request_id = Some(rid.clone());
                break;
            }
        }
    }
    let request_id = request_id.expect("request_mode must force-park");
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("not now".into()),
        })
        .await
        .unwrap();

    let events = collect_until_done(&mut sub, &sid, Duration::from_secs(3)).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ModeChanged { mode, .. } if mode == "build")),
        "a rejected request must never switch the mode: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool, output, is_error, .. }
                if tool == REQUEST_MODE_TOOL && *is_error && output.contains("not now")
        )),
        "rejection must fold the typed reason back: {events:?}"
    );
}

// --- 3. Narrowing errors, never parks -----------------------------------

#[tokio::test]
async fn narrowing_is_refused_with_no_approval_prompt() {
    let scripted = Arc::new(vec![
        request_mode_call("r1", "research"),
        text_response("ok"),
    ]);
    let holly = spawn_with_builtin_modes(Arc::new(move || {
        Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
    }));
    let sid = SessionId::new("s1");
    let events = run_one_call(&holly, &sid, "build").await;
    assert!(
        !has_tool_request(&events),
        "narrowing must never park an approval: {events:?}"
    );
    assert!(
        is_error_output(&events, "/mode"),
        "narrowing must point at /mode as the user's own action: {events:?}"
    );
}

// --- 4. `auto` target always errors, from any mode ----------------------

#[tokio::test]
async fn requesting_auto_is_always_refused() {
    for from in ["research", "plan", "build"] {
        let scripted = Arc::new(vec![request_mode_call("r1", "auto"), text_response("ok")]);
        let holly = spawn_with_builtin_modes(Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }));
        let sid = SessionId::new("s1");
        let events = run_one_call(&holly, &sid, from).await;
        assert!(
            !has_tool_request(&events),
            "requesting auto from `{from}` must never park: {events:?}"
        );
        assert!(
            is_error_output(&events, "auto"),
            "requesting auto from `{from}` must be refused, naming auto: {events:?}"
        );
    }
}

// --- 5. In `auto` mode, refused outright without parking -----------------

#[tokio::test]
async fn in_auto_mode_every_request_errors_without_parking() {
    for target in ["plan", "build", "research"] {
        let scripted = Arc::new(vec![request_mode_call("r1", target), text_response("ok")]);
        let holly = spawn_with_builtin_modes(Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }));
        let sid = SessionId::new("s1");
        let events = run_one_call(&holly, &sid, "auto").await;
        assert!(
            !has_tool_request(&events),
            "auto mode must never park a request_mode approval (target `{target}`): {events:?}"
        );
        assert!(
            is_error_output(&events, "auto"),
            "auto mode must refuse outright (target `{target}`): {events:?}"
        );
    }
}

// --- Malformed input --------------------------------------------------

#[tokio::test]
async fn missing_mode_field_is_refused_with_no_approval_prompt() {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "r1".into(),
                name: REQUEST_MODE_TOOL.into(),
                input: serde_json::json!({"reason": "need it"}).to_string(),
                provider_meta: None,
            }],
        },
        text_response("ok"),
    ]);
    let holly = spawn_with_builtin_modes(Arc::new(move || {
        Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
    }));
    let sid = SessionId::new("s1");
    let events = run_one_call(&holly, &sid, "research").await;
    assert!(
        !has_tool_request(&events),
        "malformed input must never park: {events:?}"
    );
    assert!(
        is_error_output(&events, "mode"),
        "a missing `mode` field must be refused: {events:?}"
    );
}
