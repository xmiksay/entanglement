//! `InMsg::ForkSession` (#786, ADR-0223): a non-compacting fork copies the
//! source's history verbatim (whole, or cut after a turn) into a fresh root
//! that inherits the live model/generation/mode/overlay, replays exactly from
//! its own log, and leaves the source live and untouched.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::session::Session;
use entanglement_core::{
    content_text, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream, OutEvent,
    SessionId, ToolOverlayEntry,
};
use tokio::sync::broadcast::Receiver;

use super::successor_model::{next, start_and_switch, until_done, Harness};

type Requests = Arc<Mutex<Vec<Vec<String>>>>;

/// Records every request's message texts and answers `re: <last text>`.
struct Echo(Requests);

#[async_trait]
impl Llm for Echo {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let texts: Vec<String> = req
            .messages
            .iter()
            .map(|m| content_text(&m.content))
            .collect();
        let reply = format!("re: {}", texts.last().cloned().unwrap_or_default());
        self.0.lock().unwrap().push(texts);
        Ok(entanglement_core::stream_from_response(LlmResponse {
            text: reply,
            ..Default::default()
        }))
    }
}

fn echo_config(requests: &Requests) -> EngineConfig {
    let requests = requests.clone();
    EngineConfig {
        llm_factory: Arc::new(move || Box::new(Echo(requests.clone())) as Box<dyn Llm>),
        ..EngineConfig::default()
    }
}

async fn turn(holly: &Holly, sub: &mut Receiver<OutEvent>, sid: &SessionId, text: &str) {
    holly.send(InMsg::prompt(sid.clone(), text)).await.unwrap();
    until_done(sub, sid).await;
}

/// Fork `source` and return the fork's id plus every event it emitted up to
/// and including its `SessionForked`.
async fn fork(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    source: &SessionId,
    at_turn: Option<u32>,
) -> (SessionId, Vec<OutEvent>) {
    holly
        .send(InMsg::ForkSession {
            session: source.clone(),
            at_turn,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    loop {
        let ev = next(sub).await;
        if ev.session() == Some(source) {
            continue;
        }
        let done =
            matches!(&ev, OutEvent::SessionForked { forked_from, .. } if forked_from == source);
        events.push(ev);
        if done {
            let fork = events.last().and_then(OutEvent::session).cloned().unwrap();
            events.retain(|e| e.session() == Some(&fork));
            return (fork, events);
        }
    }
}

/// Collect `sid`'s events through its next `Done`.
async fn events_until_done(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let ev = next(sub).await;
        if ev.session() == Some(sid) {
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                return out;
            }
        }
    }
}

#[tokio::test]
async fn a_full_fork_carries_the_history_and_leaves_the_source_live() {
    let requests = Requests::default();
    let holly = Holly::spawn(echo_config(&requests));
    let src = SessionId::new("src");
    let mut sub = holly.subscribe();
    turn(&holly, &mut sub, &src, "one").await;

    let (fork_id, started) = fork(&holly, &mut sub, &src, None).await;
    assert_ne!(fork_id, src);
    assert!(started.iter().any(|e| matches!(
        e,
        OutEvent::SessionStarted {
            parent: None,
            predecessor: None,
            root: true,
            ..
        }
    )));
    assert!(
        !started.iter().any(|e| matches!(e, OutEvent::Status { state, .. } if *state != entanglement_core::AgentState::Idle)),
        "a fork starts idle: {started:?}"
    );

    turn(&holly, &mut sub, &fork_id, "fork-two").await;
    assert_eq!(
        requests.lock().unwrap().last().unwrap(),
        &["one", "re: one", "fork-two"]
    );

    // The source is still live and saw none of the fork's turn.
    turn(&holly, &mut sub, &src, "src-two").await;
    assert_eq!(
        requests.lock().unwrap().last().unwrap(),
        &["one", "re: one", "src-two"]
    );
}

#[tokio::test]
async fn a_fork_at_a_turn_keeps_only_that_prefix_and_replays_exactly() {
    let requests = Requests::default();
    let cfg = echo_config(&requests);
    let holly = Holly::spawn(cfg.clone());
    let src = SessionId::new("src");
    let mut sub = holly.subscribe();
    turn(&holly, &mut sub, &src, "one").await;
    turn(&holly, &mut sub, &src, "two").await;

    let (fork_id, mut log) = fork(&holly, &mut sub, &src, Some(1)).await;
    assert!(log.iter().any(|e| matches!(
        e,
        OutEvent::SessionForked { at_turn: Some(1), forked_from, context, .. }
            if *forked_from == src && context.len() == 2
    )));
    holly
        .send(InMsg::prompt(fork_id.clone(), "three"))
        .await
        .unwrap();
    let turn_events = events_until_done(&mut sub, &fork_id).await;
    let live = requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(live, ["one", "re: one", "three"]);

    // The fork's own log — its events plus its prompt — rebuilds the same
    // history the live fork holds (the prompt pairs with its first event, as
    // the persistence tap logs it).
    let mut records: Vec<(Option<InMsg>, OutEvent)> = log.drain(..).map(|e| (None, e)).collect();
    let mut prompt = Some(InMsg::prompt(fork_id.clone(), "three"));
    records.extend(turn_events.into_iter().map(|e| (prompt.take(), e)));
    let replayed = Session::replay(&records, &cfg, &fork_id).expect("replay");
    let texts: Vec<String> = replayed
        .ctx
        .messages()
        .iter()
        .map(|m| content_text(&m.content))
        .collect();
    assert_eq!(texts, ["one", "re: one", "three", "re: three"]);
}

#[tokio::test]
async fn a_fork_inherits_the_live_model_generation_mode_and_overlay() {
    let h = Harness::new();
    let cfg = h.config(false);
    let holly = Holly::spawn(cfg.clone());
    let src = SessionId::new("src");
    let mut sub = holly.subscribe();
    start_and_switch(&holly, &mut sub, &src).await;
    let overlay = vec![ToolOverlayEntry::allow("bash")];
    holly
        .send(InMsg::SetToolOverlay {
            session: src.clone(),
            entries: overlay.clone(),
        })
        .await
        .unwrap();
    while !matches!(next(&mut sub).await, OutEvent::ToolOverlayChanged { session, .. } if session == src)
    {
    }

    let (fork_id, log) = fork(&holly, &mut sub, &src, None).await;
    let models: Vec<&str> = log
        .iter()
        .filter_map(|e| match e {
            OutEvent::ModelChanged { model, .. } => Some(model.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["glm-b"], "the source's live model, not the pin");
    assert!(log
        .iter()
        .any(|e| matches!(e, OutEvent::ModeChanged { mode, .. } if mode == "plan")));
    assert!(log
        .iter()
        .any(|e| matches!(e, OutEvent::ToolOverlayChanged { entries, .. } if *entries == overlay)));

    turn(&holly, &mut sub, &fork_id, "go").await;
    assert_eq!(h.last_request(), (Some("glm-b".into()), Some(0.3)));

    let records: Vec<(Option<InMsg>, OutEvent)> = log.into_iter().map(|e| (None, e)).collect();
    let replayed = Session::replay(&records, &cfg, &fork_id).expect("replay");
    assert_eq!(replayed.mode, "plan");
    assert_eq!(replayed.tool_overlay, overlay);
    assert_eq!(replayed.generation.and_then(|g| g.temperature), Some(0.3));
}

#[tokio::test]
async fn a_fork_keeps_the_sources_tenant_prefix() {
    let requests = Requests::default();
    let holly = Holly::spawn(echo_config(&requests));
    let src = SessionId::new("acme:src");
    let mut sub = holly.subscribe();
    turn(&holly, &mut sub, &src, "one").await;
    let (fork_id, _) = fork(&holly, &mut sub, &src, None).await;
    assert!(fork_id.0.starts_with("acme:"), "{fork_id}");
}

#[tokio::test]
async fn fork_session_is_refused_from_the_wire() {
    let holly = Holly::spawn(EngineConfig::default());
    let err = holly
        .send_from_wire(InMsg::ForkSession {
            session: SessionId::new("s"),
            at_turn: None,
        })
        .await
        .expect_err("trusted-only");
    assert!(err.to_string().contains("fork_session"), "{err}");
}
