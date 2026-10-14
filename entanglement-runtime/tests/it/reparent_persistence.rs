//! Re-parented sub-agents survive a cold resume (ADR-0234): persisted through
//! the real file sink, then rebuilt from disk by a fresh engine. A child moved
//! under a compaction successor keeps logging to the predecessor's file; the
//! successor's file points at it, so resuming the successor restores the
//! child under it, pollable and with a byte-identical history. A sub-agent's
//! own successor logs into its parent's root file and resumes under it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    MessageRole, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::session_store::{pair_records, LogPayload, LogRecord, SessionStore};
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::ToolRegistry;
use tokio::sync::broadcast::Receiver;

const WAIT: Duration = Duration::from_secs(5);

type Requests = Arc<Mutex<Vec<String>>>;

/// Stateless, scripted by the last user message; records every request's
/// messages so a probe can compare live and resumed history.
struct Script(Requests);

#[async_trait]
impl Llm for Script {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::to_string(req.messages).unwrap());
        let answered = req
            .messages
            .last()
            .is_some_and(|m| m.role == MessageRole::Tool);
        let user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User && !m.text().starts_with('['))
            .map(|m| m.text())
            .unwrap_or_default();
        let text = |t: &str| {
            stream_from_response(LlmResponse {
                text: t.into(),
                ..Default::default()
            })
        };
        Ok(if answered {
            text("done")
        } else if user == "spawn" {
            let input = r#"{"agent":"general","prompt":"child-task","background":true}"#;
            stream_from_response(LlmResponse {
                tool_calls: vec![ToolCall::new("a1", "agent", input)],
                ..Default::default()
            })
        } else if let Some(h) = user.strip_prefix("poll-handle ") {
            let input = format!(r#"{{"handle":"{h}","timeout_secs":1}}"#);
            stream_from_response(LlmResponse {
                tool_calls: vec![ToolCall::new("p2", "poll", &input)],
                ..Default::default()
            })
        } else if let Some(h) = user.strip_prefix("send ") {
            let input = format!(r#"{{"agent_id":"{h}","prompt":"followup"}}"#);
            stream_from_response(LlmResponse {
                tool_calls: vec![ToolCall::new("s1", "agent_send", &input)],
                ..Default::default()
            })
        } else if user == "poll" {
            stream_from_response(LlmResponse {
                tool_calls: vec![ToolCall::new("p1", "poll", "{}")],
                ..Default::default()
            })
        } else {
            text(&format!("{user}-answer"))
        })
    }
}

fn engine() -> (Holly, Requests) {
    let requests: Requests = Arc::default();
    let agents = entanglement_runtime::agents::built_in_registry().expect("built-in agents");
    let r = requests.clone();
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(Script(r.clone())) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    ToolExecutor::builder(&holly, ToolRegistry::new().shared())
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .mode_table(crate::mode_support::allow_all_table())
        .spawn();
    (holly, requests)
}

async fn until(sub: &mut Receiver<OutEvent>, stop: impl Fn(&OutEvent) -> bool) -> OutEvent {
    loop {
        let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await else {
            panic!("expected event never arrived");
        };
        if stop(&ev) {
            return ev;
        }
    }
}

fn done(id: &SessionId) -> impl Fn(&OutEvent) -> bool + '_ {
    move |e| matches!(e, OutEvent::Done { session, .. } if session == id)
}

/// `root` launches a background child and both finish their turn; returns
/// the child.
async fn spawn_child(holly: &Holly, sub: &mut Receiver<OutEvent>, root: &SessionId) -> SessionId {
    holly
        .send(InMsg::prompt(root.clone(), "spawn"))
        .await
        .unwrap();
    let child = match until(
        sub,
        |e| matches!(e, OutEvent::SessionStarted { parent: Some(p), .. } if p == root),
    )
    .await
    {
        OutEvent::SessionStarted { session, .. } => session,
        _ => unreachable!("matched above"),
    };
    until(sub, done(&child)).await;
    child
}

async fn compact(holly: &Holly, source: &SessionId, successor: &SessionId, prompt: &str) {
    holly
        .send(InMsg::Spawn {
            session: successor.clone(),
            parent: None,
            predecessor: Some(source.clone()),
            agent: "general".into(),
            prompt: prompt.into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: source.clone(),
        })
        .await
        .unwrap();
}

/// Wait until `root`'s tree on disk holds `text` as a `TextDelta` of
/// `session`, then return the tree.
async fn flushed(
    store: &SessionStore,
    root: &SessionId,
    session: &SessionId,
    text: &str,
) -> Vec<LogRecord> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(records) = store.read_tree(root) {
            if records.iter().any(|r| {
                matches!(&r.payload, LogPayload::Out(OutEvent::TextDelta { session: s, text: t, .. })
                    if s == session && t == text)
            }) {
                return records;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "tap never flushed {text}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The history `session` sends on its next request.
async fn probe(holly: &Holly, requests: &Requests, session: &SessionId) -> String {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(session.clone(), "probe"))
        .await
        .unwrap();
    until(&mut sub, done(session)).await;
    requests.lock().unwrap().last().cloned().expect("a request")
}

/// Cold-resume `root` from `records` on a fresh engine and wait until
/// `child` is back under `parent`.
async fn cold_resume(
    records: &[LogRecord],
    root: &SessionId,
    child: &SessionId,
    parent: &SessionId,
) -> (Holly, Requests, Receiver<OutEvent>) {
    let (holly, requests) = engine();
    let mut sub = holly.subscribe();
    holly
        .resume(root.clone(), pair_records(records))
        .await
        .unwrap();
    until(&mut sub, |e| {
        matches!(e, OutEvent::SessionStarted { session, parent: Some(p), .. }
            if session == child && p == parent)
    })
    .await;
    (holly, requests, sub)
}

/// The handles `session`'s own `ListOperations` reports.
async fn operations(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    session: &SessionId,
) -> Vec<String> {
    holly
        .send(InMsg::ListOperations {
            correlation_id: "ops".into(),
            session: Some(session.clone()),
        })
        .await
        .unwrap();
    match until(sub, |e| matches!(e, OutEvent::OperationList { .. })).await {
        OutEvent::OperationList { operations, .. } => {
            operations.into_iter().map(|o| o.handle).collect()
        }
        _ => unreachable!("matched above"),
    }
}

async fn poll_listing(holly: &Holly, sub: &mut Receiver<OutEvent>, session: &SessionId) -> String {
    tool_output(holly, sub, session, "poll", "poll").await.0
}

/// Prompt `session` with `prompt` and return its next `tool` output.
async fn tool_output(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    session: &SessionId,
    prompt: &str,
    tool: &str,
) -> (String, bool) {
    holly
        .send(InMsg::prompt(session.clone(), prompt))
        .await
        .unwrap();
    match until(sub, |e| {
        matches!(e, OutEvent::ToolOutput { session: s, tool: t, .. } if s == session && t == tool)
    })
    .await
    {
        OutEvent::ToolOutput {
            output, is_error, ..
        } => (output, is_error),
        _ => unreachable!("matched above"),
    }
}

#[tokio::test]
async fn a_child_moved_by_a_parent_compaction_resumes_under_the_successor() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(tmp.path());
    let (live, live_requests) = engine();
    let _tap = spawn_persistence_subscriber(&live, store.clone());
    let mut sub = live.subscribe();
    let (pred, succ) = (SessionId::new("cr-pred"), SessionId::new("cr-succ"));
    let child = spawn_child(&live, &mut sub, &pred).await;
    until(&mut sub, done(&pred)).await;

    compact(&live, &pred, &succ, "").await;
    until(
        &mut sub,
        |e| matches!(e, OutEvent::ParentChanged { session, .. } if session == &child),
    )
    .await;
    // Post-move work: still logged with the child's history, in pred's file.
    live.send(InMsg::prompt(child.clone(), "more"))
        .await
        .unwrap();
    until(&mut sub, done(&child)).await;

    let records = flushed(&store, &succ, &child, "more-answer").await;
    let own = store.read(&succ).expect("succ's file");
    assert!(
        own.iter().any(|r| r.session == child
            && matches!(&r.payload, LogPayload::Adopted { from } if from == &pred)),
        "the successor's file points at the child's history"
    );
    assert!(!own
        .iter()
        .any(|r| r.session == child && matches!(r.payload, LogPayload::Out(_))));
    let live_history = probe(&live, &live_requests, &child).await;

    let (resumed, resumed_requests, mut rsub) = cold_resume(&records, &succ, &child, &succ).await;
    assert_eq!(
        resumed.session_info(&child).and_then(|i| i.parent),
        Some(succ.clone())
    );
    assert_eq!(
        operations(&resumed, &mut rsub, &succ).await,
        vec![child.0.clone()]
    );
    let listing = poll_listing(&resumed, &mut rsub, &succ).await;
    assert!(listing.contains(&child.0), "{listing}");
    assert_eq!(
        probe(&resumed, &resumed_requests, &child).await,
        live_history,
        "the resumed child sends the live history"
    );
}

#[tokio::test]
async fn a_compacted_sub_agent_resumes_under_its_parent() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(tmp.path());
    let (live, live_requests) = engine();
    let _tap = spawn_persistence_subscriber(&live, store.clone());
    let mut sub = live.subscribe();
    let root = SessionId::new("cs-root");
    let child = spawn_child(&live, &mut sub, &root).await;
    until(&mut sub, done(&root)).await;
    let child_succ = SessionId::new("cs-child-succ");

    compact(&live, &child, &child_succ, "successor-task").await;
    until(&mut sub, done(&child_succ)).await;

    // The successor is a sub-agent like any other: it logs into its
    // parent's root file and has none of its own.
    let records = flushed(&store, &root, &child_succ, "successor-task-answer").await;
    assert!(store.read(&child_succ).is_err(), "no stray root file");
    let live_history = probe(&live, &live_requests, &child_succ).await;

    let (resumed, resumed_requests, mut rsub) =
        cold_resume(&records, &root, &child_succ, &root).await;
    assert_eq!(
        resumed.session_info(&child_succ).and_then(|i| i.parent),
        Some(root.clone())
    );
    // The parent knew it by its original handle.
    assert_eq!(
        operations(&resumed, &mut rsub, &root).await,
        vec![child.0.clone()]
    );
    let listing = poll_listing(&resumed, &mut rsub, &root).await;
    assert!(listing.contains(&child.0), "{listing}");
    assert_eq!(
        probe(&resumed, &resumed_requests, &child_succ).await,
        live_history,
        "the resumed successor sends the live history"
    );
}

/// A sub-agent that compacted twice before the restart is still reachable
/// by the handle its parent was given: the resume re-teaches the engine the
/// whole lineage, and the registry keys the child by its head.
#[tokio::test]
async fn a_twice_compacted_sub_agent_keeps_its_original_handle_after_a_cold_resume() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(tmp.path());
    let (live, _requests) = engine();
    let _tap = spawn_persistence_subscriber(&live, store.clone());
    let mut sub = live.subscribe();
    let root = SessionId::new("pb-root");
    let child = spawn_child(&live, &mut sub, &root).await;
    until(&mut sub, done(&root)).await;
    let (s1, s2) = (SessionId::new("pb-s1"), SessionId::new("pb-s2"));
    compact(&live, &child, &s1, "successor-task").await;
    until(&mut sub, done(&s1)).await;
    compact(&live, &s1, &s2, "second-task").await;
    until(&mut sub, done(&s2)).await;
    let records = flushed(&store, &root, &s2, "second-task-answer").await;

    let (resumed, _r, mut rsub) = cold_resume(&records, &root, &s2, &root).await;
    assert_eq!(resumed.lineage_head(&s2), child);
    assert_eq!(resumed.successor_of(&child), Some(s2.clone()));
    assert_eq!(
        operations(&resumed, &mut rsub, &root).await,
        vec![child.0.clone()]
    );
    let (polled, err) = tool_output(
        &resumed,
        &mut rsub,
        &root,
        &format!("poll-handle {child}"),
        "poll",
    )
    .await;
    assert!(!err && !polled.contains("unknown"), "{polled}");
    let (sent, err) = tool_output(
        &resumed,
        &mut rsub,
        &root,
        &format!("send {child}"),
        "agent_send",
    )
    .await;
    assert!(!err && sent.contains("followup-answer"), "{sent}");
}

/// A crash between the move and the successor's start loses the in-memory
/// adoption pointer; resume recovers the child from the predecessor chain.
#[tokio::test]
async fn a_moved_child_resumes_even_when_its_pointer_was_never_written() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(tmp.path());
    let (live, _requests) = engine();
    let _tap = spawn_persistence_subscriber(&live, store.clone());
    let mut sub = live.subscribe();
    let (pred, succ) = (SessionId::new("np-pred"), SessionId::new("np-succ"));
    let child = spawn_child(&live, &mut sub, &pred).await;
    until(&mut sub, done(&pred)).await;
    compact(&live, &pred, &succ, "").await;
    live.send(InMsg::prompt(child.clone(), "more"))
        .await
        .unwrap();
    until(&mut sub, done(&child)).await;
    flushed(&store, &succ, &child, "more-answer").await;

    // Simulate the crash: the successor's file without its pointer.
    let without: Vec<LogRecord> = store
        .read(&succ)
        .unwrap()
        .into_iter()
        .filter(|r| !matches!(r.payload, LogPayload::Adopted { .. }))
        .collect();
    store.delete(&succ).unwrap();
    for r in &without {
        store.append(&succ, r).unwrap();
    }

    let records = store.read_tree(&succ).unwrap();
    let (resumed, _r, mut rsub) = cold_resume(&records, &succ, &child, &succ).await;
    assert_eq!(
        operations(&resumed, &mut rsub, &succ).await,
        vec![child.0.clone()]
    );
}
