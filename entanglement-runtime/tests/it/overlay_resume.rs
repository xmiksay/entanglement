//! The per-session tool overlay is enforced again after a resume (#776). The
//! runtime grades dispatch off its own copy of the overlay. That copy is built
//! only from `ToolOverlayChanged` and forgotten on hibernate, and a fresh
//! process never had it. So a resumed session must re-announce the overlay
//! core replayed. Otherwise a user's deny entry stops applying (fail-open)
//! and an enable entry is lost. This holds for a plain session (same process
//! after hibernation, and a fresh process) and for a compaction successor.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall, ToolOverlayEntry,
};
use entanglement_runtime::capability::Capability;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::{Tool, ToolRegistry};

/// Calls `edit` when the last message is exactly `"edit it"`, else answers
/// `"ok"` — stateless, so every backend instance (a resume, a successor, a
/// compaction summary) behaves the same.
struct EditOnCue;

#[async_trait]
impl Llm for EditOnCue {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let cue = req.messages.last().is_some_and(|m| m.text() == "edit it");
        Ok(stream_from_response(if cue {
            LlmResponse {
                tool_calls: vec![ToolCall::new("t1", "edit", "{\"path\":\"x\"}")],
                ..Default::default()
            }
        } else {
            LlmResponse {
                text: "ok".into(),
                ..Default::default()
            }
        }))
    }
}

/// A `Write` tool that reports when it runs.
struct EchoEdit;

#[async_trait]
impl Tool for EchoEdit {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edit")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Write]
    }
}

/// An engine wired with the real runtime executor over the built-in modes.
fn engine() -> Holly {
    let agents = entanglement_runtime::agents::built_in_registry().expect("built-in agents");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(EditOnCue) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    let mut reg = ToolRegistry::new();
    reg.register(EchoEdit);
    let _executor = ToolExecutor::builder(&holly, reg.shared())
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .spawn();
    holly
}

/// `sid`'s events up to and including the first one matching `stop`.
async fn log_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    stop: impl Fn(&OutEvent) -> bool,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out")
            .expect("event stream open");
        if ev.session() == Some(sid) {
            let hit = stop(&ev);
            out.push(ev);
            if hit {
                return out;
            }
        }
    }
}

fn is_done(e: &OutEvent) -> bool {
    matches!(e, OutEvent::Done { .. })
}

fn records(log: &[OutEvent]) -> Vec<(Option<InMsg>, OutEvent)> {
    log.iter().cloned().map(|e| (None, e)).collect()
}

/// Resume `sid` from `log` on `holly`, ask for the edit, return its outputs.
async fn edit_after_resume(holly: &Holly, sid: &SessionId, log: &[OutEvent]) -> Vec<String> {
    let mut sub = holly.subscribe();
    holly.resume(sid.clone(), records(log)).await.unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "edit it"))
        .await
        .unwrap();
    log_until(&mut sub, sid, is_done)
        .await
        .into_iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output),
            _ => None,
        })
        .collect()
}

/// Set `entries` on `sid` (spawning it), run one turn, return its log.
async fn session_with_overlay(
    holly: &Holly,
    sid: &SessionId,
    mode: Option<&str>,
    entries: Vec<ToolOverlayEntry>,
) -> Vec<OutEvent> {
    let mut sub = holly.subscribe();
    if let Some(mode) = mode {
        holly
            .send(InMsg::SetMode {
                session: sid.clone(),
                mode: mode.into(),
            })
            .await
            .unwrap();
    }
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries,
        })
        .await
        .unwrap();
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    log_until(&mut sub, sid, is_done).await
}

fn assert_declined(outs: &[String]) {
    assert!(
        outs.iter()
            .any(|o| o.contains("withdrawn for this session")),
        "the deny entry must still decline after resume; got {outs:?}"
    );
    assert!(!outs.iter().any(|o| o.starts_with("ran:")), "{outs:?}");
}

/// Hibernate makes the runtime forget the overlay. The deny must apply again
/// once the session resumes in the same process.
#[tokio::test]
async fn a_deny_entry_still_applies_after_hibernate_and_resume() {
    let holly = engine();
    let sid = SessionId::new("s1");
    let log = session_with_overlay(&holly, &sid, None, vec![ToolOverlayEntry::deny("edit")]).await;

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::HibernateSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    log_until(&mut sub, &sid, |e| {
        matches!(e, OutEvent::SessionHibernated { .. })
    })
    .await;

    assert_declined(&edit_after_resume(&holly, &sid, &log).await);
}

/// A fresh process never saw the overlay. An enable that beats `research`'s
/// write deny must still be honored after resume.
#[tokio::test]
async fn an_enable_entry_is_honored_after_resume_in_a_fresh_process() {
    let sid = SessionId::new("s1");
    let log = session_with_overlay(
        &engine(),
        &sid,
        Some("research"),
        vec![ToolOverlayEntry::allow("edit")],
    )
    .await;

    let outs = edit_after_resume(&engine(), &sid, &log).await;
    assert!(
        outs.iter().any(|o| o.starts_with("ran:")),
        "the enable must override research's write deny after resume; got {outs:?}"
    );
}

/// A compaction successor inherits the deny, and keeps enforcing it after its
/// own resume in a fresh process.
#[tokio::test]
async fn a_successors_inherited_deny_still_applies_after_resume() {
    let holly = engine();
    let sid = SessionId::new("s1");
    session_with_overlay(&holly, &sid, None, vec![ToolOverlayEntry::deny("edit")]).await;

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Oneshot {
            session: sid.clone(),
            op: "compact".into(),
            args: serde_json::Value::Null,
        })
        .await
        .unwrap();
    let successor = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out")
            .expect("event stream open");
        if let OutEvent::SessionStarted {
            session,
            predecessor: Some(p),
            ..
        } = &ev
        {
            if *p == sid {
                break (session.clone(), ev);
            }
        }
    };
    let (successor, started) = successor;
    let mut log = vec![started];
    log.extend(log_until(&mut sub, &successor, is_done).await);

    assert_declined(&edit_after_resume(&engine(), &successor, &log).await);
}
