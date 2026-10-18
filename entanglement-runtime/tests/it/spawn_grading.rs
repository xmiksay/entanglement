//! `agent`/`agent_send` are graded like any tool (#849, ADR-0232): the
//! session overlay and the mode's rules reach them through the same dispatch
//! ladder every host tool climbs. A refusal is an ordinary error result the
//! model sees, and no child session is ever started.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    MessageRole, OutEvent, Permission, PermissionProfile, SessionId, ToolCall, ToolOverlayEntry,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::tool_names::{AGENT_SEND_TOOL, AGENT_TOOL};
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::ToolRegistry;

const SPAWN: &str = r#"{"agent":"general","prompt":"child-task","background":true}"#;
const SEND: &str = r#"{"agent_id":"no-such-child","prompt":"follow-up"}"#;

/// The scripted parent: one tool call per round, in order, then plain text.
/// A child session (its task is `child-task`) answers `child-answer` at once.
/// Every session gets a fresh copy from the factory.
struct Scripted {
    calls: Mutex<Vec<ToolCall>>,
}

#[async_trait]
impl Llm for Scripted {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let is_child = req
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.text() == "child-task");
        let call = if is_child {
            None
        } else {
            let mut calls = self.calls.lock().unwrap();
            (!calls.is_empty()).then(|| calls.remove(0))
        };
        Ok(stream_from_response(LlmResponse {
            text: match (&call, is_child) {
                (Some(_), _) => String::new(),
                (None, true) => "child-answer".into(),
                (None, false) => "done".into(),
            },
            tool_calls: call.into_iter().collect(),
            ..Default::default()
        }))
    }
}

/// A single `build` mode (the default a fresh session resolves against)
/// with `default: allow`, `rules` and `limits`.
pub(crate) fn table_with(deny: &[&str], prompt: &[&str], limits: Limits) -> Arc<ModeTable> {
    let s = |l: &[&str]| l.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    let mode = Mode {
        name: "build".to_string(),
        default: Permission::Allow,
        rules: Rules::from_lists(&s(deny), &[], &s(prompt)),
        limits,
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
}

fn table(deny: &[&str], prompt: &[&str]) -> Arc<ModeTable> {
    table_with(deny, prompt, Limits::default())
}

/// An engine whose parent sessions make `calls` (tool, input) in order.
pub(crate) fn spawn_calls(calls: &[(&'static str, &'static str)], table: Arc<ModeTable>) -> Holly {
    let calls: Vec<ToolCall> = calls
        .iter()
        .enumerate()
        .map(|(i, (tool, input))| ToolCall {
            id: format!("c{i}"),
            name: (*tool).into(),
            input: (*input).into(),
            provider_meta: None,
        })
        .collect();
    let agents = entanglement_runtime::agents::built_in_registry().expect("built-ins parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(Scripted {
                calls: Mutex::new(calls.clone()),
            }) as Box<dyn Llm>
        }),
        agents: agents.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    ToolExecutor::builder(&holly, ToolRegistry::new().shared())
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .mode_table(table)
        .spawn();
    holly
}

fn spawn(tool: &'static str, input: &'static str, table: Arc<ModeTable>) -> Holly {
    spawn_calls(&[(tool, input)], table)
}

async fn set_overlay(holly: &Holly, sid: &SessionId, entries: Vec<ToolOverlayEntry>) {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries,
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolOverlayChanged { .. })) => return,
            Ok(Ok(_)) => {}
            other => panic!("no overlay confirmation: {other:?}"),
        }
    }
}

/// Every event until `sid`'s turn ends (the bounded per-event timeout also
/// ends it), from every session — a child start must be visible.
pub(crate) async fn run_turn(holly: &Holly, sid: &SessionId) -> Vec<OutEvent> {
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == sid);
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

/// The call came back an error result containing `needle`, and nothing ran.
pub(crate) fn assert_refused(events: &[OutEvent], tool: &str, needle: &str) {
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { tool: t, output, is_error: true, .. }
                if t == tool && output.contains(needle)
        )),
        "`{tool}` must be refused with `{needle}`: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            OutEvent::SessionStarted {
                parent: Some(_),
                ..
            }
        )),
        "a refused `{tool}` must start no sub-session: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a refusal is flat, no approval card: {events:?}"
    );
}

/// `/disable agent` (the TUI upserts exactly this wire deny entry) means no
/// spawning.
#[tokio::test]
async fn an_overlay_deny_on_agent_refuses_the_spawn() {
    let holly = spawn(AGENT_TOOL, SPAWN, table(&[], &[]));
    let sid = SessionId::new("s1");
    set_overlay(&holly, &sid, vec![ToolOverlayEntry::deny(AGENT_TOOL)]).await;
    let events = run_turn(&holly, &sid).await;
    assert_refused(
        &events,
        AGENT_TOOL,
        "withdrawn for this session by /disable",
    );
}

#[tokio::test]
async fn a_mode_deny_rule_on_agent_refuses_the_spawn() {
    let holly = spawn(AGENT_TOOL, SPAWN, table(&[AGENT_TOOL], &[]));
    let events = run_turn(&holly, &SessionId::new("s1")).await;
    assert_refused(&events, AGENT_TOOL, "denied by mode `build`");
}

/// A `control` class deny reaches spawning too — `agent` is `Control`.
#[tokio::test]
async fn a_control_class_deny_refuses_the_spawn() {
    let holly = spawn(AGENT_TOOL, SPAWN, table(&["control"], &[]));
    let events = run_turn(&holly, &SessionId::new("s1")).await;
    assert_refused(&events, AGENT_TOOL, "denied by mode `build`");
}

/// The control case: the same harness with no rule spawns a child.
#[tokio::test]
async fn with_no_rule_the_spawn_starts_a_child() {
    let holly = spawn(AGENT_TOOL, SPAWN, table(&[], &[]));
    let events = run_turn(&holly, &SessionId::new("s1")).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::SessionStarted {
                parent: Some(_),
                ..
            }
        )),
        "an allowed spawn starts a child: {events:?}"
    );
}

/// A `prompt:` rule on `agent` asks; rejecting the card spawns nothing.
#[tokio::test]
async fn a_mode_prompt_rule_on_agent_asks_first() {
    let holly = spawn(AGENT_TOOL, SPAWN, table(&[], &[AGENT_TOOL]));
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let request_id = loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                request_id, tool, ..
            })) => {
                assert_eq!(tool, AGENT_TOOL);
                break request_id;
            }
            Ok(Ok(OutEvent::SessionStarted {
                parent: Some(_), ..
            })) => panic!("no child may start before the card is answered"),
            Ok(Ok(_)) => {}
            other => panic!("expected an approval card for `agent`: {other:?}"),
        }
    };
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("not now".into()),
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        let done = matches!(&ev, OutEvent::Done { session, .. } if session == &sid);
        events.push(ev);
        if done {
            break;
        }
    }
    assert_refused(&events, AGENT_TOOL, "rejected: not now");
}

/// `agent_send` is graded before the child lookup: the refusal names the
/// overlay, not the unknown `agent_id`.
#[tokio::test]
async fn an_overlay_deny_on_agent_send_refuses_it() {
    let holly = spawn(AGENT_SEND_TOOL, SEND, table(&[], &[]));
    let sid = SessionId::new("s1");
    set_overlay(&holly, &sid, vec![ToolOverlayEntry::deny(AGENT_SEND_TOOL)]).await;
    let events = run_turn(&holly, &sid).await;
    assert_refused(
        &events,
        AGENT_SEND_TOOL,
        "withdrawn for this session by /disable",
    );
}

#[tokio::test]
async fn a_mode_deny_rule_on_agent_send_refuses_it() {
    let holly = spawn(AGENT_SEND_TOOL, SEND, table(&[AGENT_SEND_TOOL], &[]));
    let events = run_turn(&holly, &SessionId::new("s1")).await;
    assert_refused(&events, AGENT_SEND_TOOL, "denied by mode `build`");
}
