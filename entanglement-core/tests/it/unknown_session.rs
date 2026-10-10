//! The unknown-session policy (#721, ADR-0210): what a routable message for an
//! id with no live task does under `SpawnBlank` (default), `Refuse` and
//! `Resume(loader)`, plus `Holly::is_live`. Drives `Holly` end-to-end; the
//! "store" is an in-memory map, never the real data dir.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, OutEvent, SessionId, SessionLoader, UnknownSession, UserId,
};
use tokio::sync::{broadcast, Semaphore};

pub(crate) type Records = Vec<(Option<InMsg>, OutEvent)>;
type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

struct RecordingLlm(Seen);

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.0.lock().unwrap().push(req.messages.to_vec());
        Ok(stream_from_response(LlmResponse {
            text: "reply".into(),
            tool_calls: vec![],
            ..Default::default()
        }))
    }
}

/// An in-memory store: `logs` answers `load`, `fail` makes it error, and
/// `panic` makes it panic, and `gate` (when set) holds every load until the
/// test releases a permit.
#[derive(Default)]
pub(crate) struct MapLoader {
    pub(crate) logs: Mutex<HashMap<SessionId, Records>>,
    pub(crate) calls: AtomicUsize,
    pub(crate) fail: bool,
    pub(crate) panic: bool,
    pub(crate) gate: Option<Arc<Semaphore>>,
}

#[async_trait]
impl SessionLoader for MapLoader {
    async fn load(&self, session: &SessionId) -> anyhow::Result<Option<Records>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            let _permit = gate.acquire().await?;
        }
        assert!(!self.panic, "loader blew up");
        if self.fail {
            anyhow::bail!("store offline");
        }
        Ok(self.logs.lock().unwrap().get(session).cloned())
    }
}

pub(crate) fn engine(policy: UnknownSession) -> (Holly, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || Box::new(RecordingLlm(seen2.clone())) as Box<dyn Llm>),
        unknown_session: policy,
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), seen)
}

pub(crate) async fn recv_until(
    sub: &mut broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

/// Every event for `sid` until `Done`, with `first_in` paired onto the first
/// one — the shape the runtime's `pair_records` hands `Holly::resume`.
async fn record_until_done(
    sub: &mut broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    first_in: InMsg,
) -> Records {
    let mut records = Vec::new();
    let mut pending = Some(first_in);
    loop {
        let ev = recv_until(sub, |e| e.session() == Some(sid)).await;
        let done = matches!(ev, OutEvent::Done { .. });
        records.push((pending.take(), ev));
        if done {
            return records;
        }
    }
}

/// A one-turn log for `sid` run on a throwaway engine, as a store would hold it.
pub(crate) async fn one_turn_log(sid: &SessionId) -> Records {
    let (holly, _) = engine(UnknownSession::SpawnBlank);
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "one")).await.unwrap();
    record_until_done(&mut sub, sid, InMsg::prompt(sid.clone(), "one")).await
}

pub(crate) fn is_done(sid: &SessionId) -> impl Fn(&OutEvent) -> bool + '_ {
    move |e| matches!(e, OutEvent::Done { session, .. } if session == sid)
}

pub(crate) fn error_text(ev: OutEvent) -> String {
    match ev {
        OutEvent::Error { seq, message, .. } => {
            assert_eq!(seq, 0, "a refusal for a non-live id rides the seq-0 bypass");
            message
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Assert nothing was spawned for `sid`: no `SessionStarted` arrives within a
/// short window and the engine reports it not live.
pub(crate) async fn assert_nothing_spawned(
    holly: &Holly,
    sub: &mut broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) {
    let started = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted { session, .. }) if &session == sid => return,
                Ok(_) => {}
                Err(_) => std::future::pending::<()>().await,
            }
        }
    })
    .await;
    assert!(started.is_err(), "no session may be spawned for {sid}");
    assert!(!holly.is_live(sid));
}

#[tokio::test]
async fn spawn_blank_default_lazily_creates_a_session() {
    let (holly, seen) = engine(UnknownSession::default());
    let mut sub = holly.subscribe();
    let sid = SessionId::new("fresh");
    assert!(!holly.is_live(&sid));
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    recv_until(&mut sub, is_done(&sid)).await;
    assert!(holly.is_live(&sid), "a lazily spawned session is live");
    assert_eq!(
        seen.lock().unwrap()[0].len(),
        1,
        "a blank session has no history"
    );
}

#[tokio::test]
async fn refuse_emits_an_error_and_spawns_nothing() {
    let (holly, _) = engine(UnknownSession::Refuse);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("ghost");
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    let ev = recv_until(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
    assert_eq!(ev.session(), Some(&sid));
    assert!(
        error_text(ev).contains("ghost"),
        "the error names the session"
    );
    assert_nothing_spawned(&holly, &mut sub, &sid).await;
}

#[tokio::test]
async fn resume_loader_rebuilds_history_then_delivers_the_prompt() {
    let sid = SessionId::new("stored");
    let loader = Arc::new(MapLoader::default());
    let log = one_turn_log(&sid).await;
    loader.logs.lock().unwrap().insert(sid.clone(), log);
    let (holly, seen) = engine(UnknownSession::Resume(loader.clone()));
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    recv_until(&mut sub, is_done(&sid)).await;

    assert_eq!(loader.calls.load(Ordering::SeqCst), 1);
    let call = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        call.len(),
        3,
        "[user one, assistant reply, user two]; got {call:?}"
    );
    assert!(holly.is_live(&sid));
}

#[tokio::test]
async fn loader_none_behaves_as_refuse() {
    let loader = Arc::new(MapLoader::default());
    let (holly, _) = engine(UnknownSession::Resume(loader.clone()));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("missing");
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    let ev = recv_until(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
    assert!(error_text(ev).contains("missing"));
    assert_eq!(loader.calls.load(Ordering::SeqCst), 1);
    assert_nothing_spawned(&holly, &mut sub, &sid).await;
}

#[tokio::test]
async fn loader_error_surfaces_and_spawns_nothing() {
    let loader = Arc::new(MapLoader {
        fail: true,
        ..MapLoader::default()
    });
    let (holly, _) = engine(UnknownSession::Resume(loader));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("broken");
    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    let ev = recv_until(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
    let text = error_text(ev);
    assert!(
        text.contains("broken") && text.contains("store offline"),
        "{text}"
    );
    assert_nothing_spawned(&holly, &mut sub, &sid).await;
}

#[tokio::test]
async fn concurrent_messages_queue_behind_one_load_and_deliver_in_order() {
    let sid = SessionId::new("busy");
    let gate = Arc::new(Semaphore::new(0));
    let loader = Arc::new(MapLoader {
        gate: Some(gate.clone()),
        ..MapLoader::default()
    });
    let log = one_turn_log(&sid).await;
    loader.logs.lock().unwrap().insert(sid.clone(), log);
    let (holly, seen) = engine(UnknownSession::Resume(loader.clone()));
    let mut sub = holly.subscribe();

    for name in ["a", "b", "c"] {
        holly
            .send(InMsg::SetSessionMeta {
                session: sid.clone(),
                name: Some(name.into()),
                action: None,
                if_unset: false,
            })
            .await
            .unwrap();
    }
    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    // Every message is already queued behind the gated load; release it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !holly.is_live(&sid),
        "nothing runs until the load completes"
    );
    gate.add_permits(10);

    let mut names = Vec::new();
    loop {
        match recv_until(&mut sub, |e| e.session() == Some(&sid)).await {
            OutEvent::SessionMetaChanged { name, .. } => names.extend(name),
            OutEvent::Done { .. } => break,
            _ => {}
        }
    }
    assert_eq!(
        names,
        ["a", "b", "c"],
        "queued messages deliver in arrival order"
    );
    assert_eq!(
        loader.calls.load(Ordering::SeqCst),
        1,
        "exactly one load per id"
    );
    let call = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(call.len(), 3, "the prompt ran on the resumed history");
}

#[tokio::test]
async fn hibernated_session_resumes_with_its_user_preserved() {
    let loader = Arc::new(MapLoader::default());
    let (holly, seen) = engine(UnknownSession::Resume(loader.clone()));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("alice:1");
    let alice = UserId::new("alice");

    let spawn = InMsg::Spawn {
        session: sid.clone(),
        parent: None,
        predecessor: None,
        agent: "general".into(),
        prompt: "one".into(),
        user: Some(alice.clone()),
        model: None,
        mode: None,
    };
    holly.send(spawn).await.unwrap();
    // The runtime's tap synthesizes a spawn's prompt as an `In` Prompt (ADR-0113).
    let log = record_until_done(&mut sub, &sid, InMsg::prompt(sid.clone(), "one")).await;
    assert!(holly.is_live(&sid));
    loader.logs.lock().unwrap().insert(sid.clone(), log);

    holly.hibernate(sid.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == sid),
    )
    .await;
    assert!(!holly.is_live(&sid), "a hibernated session is not live");

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
    let OutEvent::SessionStarted { user, .. } = ev else {
        unreachable!()
    };
    assert_eq!(user, Some(alice), "the resumed session keeps its user");
    recv_until(&mut sub, is_done(&sid)).await;
    let call = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(call.len(), 3, "resumed, not blank-respawned; got {call:?}");
    assert!(holly.is_live(&sid));
}
