//! Integration tests for the runtime-owned `agent_send` tool (#609, ADR-0162):
//! sending a follow-up prompt to a sub-agent already launched with `agent`.
//!
//! Mirrors `tests/propose_plan.rs`'s harness (`spawn_with_root`, a real
//! `Holly` + tool executor). The `build` profile is used as the root — it's
//! `DEFAULT_PROFILE` (`entanglement_core::holly`), inherit-all masked, and
//! `mode: primary` so it may spawn — the same lazy-`Prompt` path
//! `propose_plan`'s tests rely on.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    content_text, stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::hooks::Hooks;
use entanglement_runtime::host::host_tools_with_extra_roots;
use entanglement_runtime::policy::{DefaultGrantStore, ProfileResolver, SandboxConfig};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_names::{AGENT_SEND_TOOL, AGENT_TOOL};
use entanglement_runtime::tool_runner::{spawn_tool_executor_with_policy, EscapeRoot};

/// Replays scripted responses in order, then plain text so the turn terminates.
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

/// A step-indexed `Llm` for the root session: step 0 spawns a `debug` child
/// (`agent { background: true }`), step 1 ends that first turn with plain
/// text, step 2 reads the child's `agent_id` back out of the prior tool
/// result (folded into `req.messages`) and sends it a follow-up via
/// `agent_send`, step 3+ ends the second turn with plain text. Mirrors
/// `agent_call`'s fixed-input shape everywhere except the `agent_send` step,
/// whose `agent_id` argument isn't known until the child is actually minted.
struct RootLlm {
    step: usize,
    background: bool,
}
impl RootLlm {
    fn new() -> Self {
        Self {
            step: 0,
            background: false,
        }
    }
}
#[async_trait]
impl Llm for RootLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let step = self.step;
        self.step += 1;
        let resp = match step {
            0 => agent_call(
                "a1",
                serde_json::json!({"agent": "debug", "prompt": "investigate X", "background": true}),
            ),
            1 => text_response("spawned; will check on it next turn"),
            2 => {
                let agent_id = extract_agent_id(req.messages)
                    .expect("agent_id must appear in a prior tool result");
                agent_send_call(
                    "a2",
                    serde_json::json!({
                        "agent_id": agent_id,
                        "prompt": "focus on Y instead",
                        "background": self.background,
                    }),
                )
            }
            _ => text_response("root done"),
        };
        Ok(stream_from_response(resp))
    }
}

/// Pull the `agent_id: <id>` token out of the most recent tool-result message
/// — the shape both `agent { background: true }`'s reply and `poll`'s agent
/// arm use.
fn extract_agent_id(messages: &[entanglement_core::Message]) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        let text = content_text(&m.content);
        let idx = text.find("agent_id: ")?;
        text[idx + "agent_id: ".len()..]
            .split(|c: char| c == '.' || c.is_whitespace())
            .next()
            .map(str::to_string)
    })
}

fn agent_call(id: &str, input: serde_json::Value) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: AGENT_TOOL.into(),
            input: input.to_string(),
            provider_meta: None,
        }],
    }
}

fn agent_send_call(id: &str, input: serde_json::Value) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: AGENT_SEND_TOOL.into(),
            input: input.to_string(),
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

fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("entanglement-agent-send-it-")
        .tempdir()
        .unwrap()
}

/// Spawn a `Holly` + real tool executor rooted at `root` (mirrors
/// `tests/propose_plan.rs`'s harness of the same name).
fn spawn_with_root(root: &Path, llm_factory: Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>) -> Holly {
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory,
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let store = Arc::new(ExtraRootStore::ephemeral());
    let tools = host_tools_with_extra_roots(root.to_path_buf(), Some(store.clone()));
    let base = entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let resolver = Arc::new(ProfileResolver::new(
        active.clone(),
        base.clone(),
        Some(root.to_path_buf()),
    ));
    let grants = Arc::new(DefaultGrantStore::load());
    let escape_root = EscapeRoot {
        root: root.to_path_buf(),
        store,
    };
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        tools.shared(),
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        base,
        active,
        resolver,
        grants,
        Hooks::default(),
        Some(escape_root),
        SandboxConfig::none(),
    );
    holly
}

/// Build a two-session `llm_factory`: the first session created (the root,
/// via the lazy-`Prompt` path) gets `root_llm`; the second (the spawned
/// child) gets a fresh `ScriptedLlm` over `child_responses`.
fn two_session_factory(
    root_llm: Arc<Mutex<Option<RootLlm>>>,
    child_responses: Vec<LlmResponse>,
) -> Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync> {
    let child_responses = Arc::new(child_responses);
    Arc::new(move || {
        if let Some(root) = root_llm.lock().unwrap().take() {
            Box::new(root) as Box<dyn Llm>
        } else {
            Box::new(ScriptedLlm::new((*child_responses).clone())) as Box<dyn Llm>
        }
    })
}

/// Collect events for `sid` until its turn ends (`Done`).
async fn collect_until_done(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    timeout: Duration,
) -> Vec<OutEvent> {
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(timeout, sub.recv()).await {
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == sid);
        if ev.session() == Some(sid) || done {
            events.push(ev);
        }
        if done {
            break;
        }
    }
    events
}

fn tool_output_for<'a>(events: &'a [OutEvent], tool: &str) -> Option<&'a str> {
    events.iter().find_map(|ev| match ev {
        OutEvent::ToolOutput {
            tool: t, output, ..
        } if t == tool => Some(output.as_str()),
        _ => None,
    })
}

/// #609: the common case — a parent that launched a child with
/// `agent { background: true }` steers it with `agent_send`, blocks for the
/// child's *next* answer (not its first), and the child actually receives the
/// new prompt and answers to it.
#[tokio::test]
async fn agent_send_steers_a_child_and_folds_the_new_answer_back() {
    let dir = tempdir();
    let root = dir.path();
    let root_llm = Arc::new(Mutex::new(Some(RootLlm::new())));
    let child_responses = vec![
        text_response("child initial finding: still unclear"),
        text_response("child refined finding: it's Y"),
    ];
    let holly = spawn_with_root(root, two_session_factory(root_llm, child_responses));
    let sid = SessionId::new("root1");
    let mut sub = holly.subscribe();

    // Turn 1: root spawns the child and ends its turn.
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn1 = collect_until_done(&mut sub, &sid, Duration::from_secs(5)).await;
    let agent_output = tool_output_for(&turn1, AGENT_TOOL).expect("agent tool must reply");
    assert!(
        agent_output.contains("agent_id:"),
        "background launch must hand back a handle: {agent_output}"
    );

    // Turn 2: root calls agent_send against the captured child, blocking for
    // its *next* answer.
    holly
        .send(InMsg::prompt(sid.clone(), "check on it"))
        .await
        .unwrap();
    let turn2 = collect_until_done(&mut sub, &sid, Duration::from_secs(5)).await;
    let send_output = tool_output_for(&turn2, AGENT_SEND_TOOL).expect("agent_send tool must reply");
    assert!(
        send_output.contains("it's Y"),
        "agent_send must fold the child's *new* answer back, not the first one: {send_output}"
    );
    assert!(
        !send_output.contains("still unclear"),
        "must not return the child's stale first answer: {send_output}"
    );
}

/// #609, ADR-0162 §4: an `agent_id` this session never launched (or that
/// never existed) is refused clearly, with no engine side effect.
#[tokio::test]
async fn agent_send_refuses_an_unknown_handle() {
    let dir = tempdir();
    let root = dir.path();
    let holly = spawn_with_root(
        root,
        Arc::new(|| {
            Box::new(ScriptedLlm::new(vec![agent_send_call(
                "a1",
                serde_json::json!({"agent_id": "s-nonexistent", "prompt": "hi"}),
            )])) as Box<dyn Llm>
        }),
    );
    let sid = SessionId::new("root2");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn = collect_until_done(&mut sub, &sid, Duration::from_secs(5)).await;
    let output = tool_output_for(&turn, AGENT_SEND_TOOL).expect("agent_send tool must reply");
    assert!(output.contains("unknown agent_id"), "{output}");
}

/// #609, ADR-0162 §4: a hibernated child must refuse loudly, never silently
/// respawn blank. Hibernating for real (`Holly::hibernate`) exercises the
/// tool executor's `SessionHibernated` → `AgentRegistry::mark_hibernated`
/// wiring, not just the registry's own unit-level contract.
#[tokio::test]
async fn agent_send_refuses_a_hibernated_child() {
    let dir = tempdir();
    let root = dir.path();
    let root_llm = Arc::new(Mutex::new(Some(RootLlm::new())));
    let child_responses = vec![text_response("child initial finding")];
    let holly = spawn_with_root(root, two_session_factory(root_llm, child_responses));
    let sid = SessionId::new("root3");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn1 = collect_until_done(&mut sub, &sid, Duration::from_secs(5)).await;
    let agent_output = tool_output_for(&turn1, AGENT_TOOL).expect("agent tool must reply");
    let child_id = agent_output
        .split("agent_id: ")
        .nth(1)
        .and_then(|s| s.split(|c: char| c == '.' || c.is_whitespace()).next())
        .expect("agent_id in the launch reply");

    holly
        .hibernate(SessionId::new(child_id))
        .await
        .expect("hibernate must succeed");
    // Let the SessionHibernated broadcast actually land before agent_send runs.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(OutEvent::SessionHibernated { session, .. }) = sub.recv().await {
                if session == SessionId::new(child_id) {
                    break;
                }
            }
        }
    })
    .await
    .expect("SessionHibernated must fire");

    holly
        .send(InMsg::prompt(sid.clone(), "check on it"))
        .await
        .unwrap();
    let turn2 = collect_until_done(&mut sub, &sid, Duration::from_secs(5)).await;
    let output = tool_output_for(&turn2, AGENT_SEND_TOOL).expect("agent_send tool must reply");
    assert!(output.contains("hibernated"), "{output}");
}

/// #609, ADR-0162 §4: a closed (tombstoned) child refuses clearly — the id is
/// spent and can never run again.
#[tokio::test]
async fn agent_send_refuses_a_closed_child() {
    let dir = tempdir();
    let root = dir.path();
    let root_llm = Arc::new(Mutex::new(Some(RootLlm::new())));
    let child_responses = vec![text_response("child initial finding")];
    let holly = spawn_with_root(root, two_session_factory(root_llm, child_responses));
    let sid = SessionId::new("root4");
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn1 = collect_until_done(&mut sub, &sid, Duration::from_secs(5)).await;
    let agent_output = tool_output_for(&turn1, AGENT_TOOL).expect("agent tool must reply");
    let child_id = agent_output
        .split("agent_id: ")
        .nth(1)
        .and_then(|s| s.split(|c: char| c == '.' || c.is_whitespace()).next())
        .expect("agent_id in the launch reply")
        .to_string();

    holly
        .send(InMsg::CloseSession {
            session: SessionId::new(&child_id),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(OutEvent::SessionEnded { session, .. }) = sub.recv().await {
                if session == SessionId::new(&child_id) {
                    break;
                }
            }
        }
    })
    .await
    .expect("SessionEnded must fire");

    holly
        .send(InMsg::prompt(sid.clone(), "check on it"))
        .await
        .unwrap();
    let turn2 = collect_until_done(&mut sub, &sid, Duration::from_secs(5)).await;
    let output = tool_output_for(&turn2, AGENT_SEND_TOOL).expect("agent_send tool must reply");
    assert!(output.contains("closed"), "{output}");
}
