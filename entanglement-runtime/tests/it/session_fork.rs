//! `/fork` through the persistence tap (#786, ADR-0223): the fork gets its own
//! root log that replays to exactly the history it started from — the
//! source's turns up to the cut, no blank seed prompt — while the source's log
//! records no fork history and the source keeps running.

use std::time::Duration;

use entanglement_core::{
    content_text, session::Session, EngineConfig, Holly, InMsg, OutEvent, SessionId,
};
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::session_store::{pair_records, LogPayload, LogRecord, SessionStore};
use tokio::sync::broadcast::Receiver;

async fn until(sub: &mut Receiver<OutEvent>, pred: impl Fn(&OutEvent) -> bool) -> OutEvent {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("timed out")
            .expect("event");
        if pred(&ev) {
            return ev;
        }
    }
}

async fn turn(holly: &Holly, sub: &mut Receiver<OutEvent>, sid: &SessionId, text: &str) {
    holly.send(InMsg::prompt(sid.clone(), text)).await.unwrap();
    until(
        sub,
        |e| matches!(e, OutEvent::Done { session, .. } if session == sid),
    )
    .await;
}

fn texts(s: &Session) -> Vec<String> {
    s.ctx
        .messages()
        .iter()
        .map(|m| content_text(&m.content))
        .collect()
}

#[tokio::test]
async fn a_fork_at_a_turn_persists_its_own_exactly_replayable_log() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::at(tmp.path());
    let cfg = EngineConfig::default();
    let holly = Holly::spawn(cfg.clone());
    let _tap = spawn_persistence_subscriber(&holly, store.clone());
    let mut sub = holly.subscribe();
    let src = SessionId::new("src");
    turn(&holly, &mut sub, &src, "one").await;
    turn(&holly, &mut sub, &src, "two").await;

    holly
        .send(InMsg::ForkSession {
            session: src.clone(),
            at_turn: Some(1),
        })
        .await
        .unwrap();
    let fork = until(&mut sub, |e| matches!(e, OutEvent::SessionForked { .. }))
        .await
        .session()
        .cloned()
        .unwrap();
    turn(&holly, &mut sub, &fork, "three").await;
    // The source is still live: another turn of its own runs.
    turn(&holly, &mut sub, &src, "four").await;
    holly.shutdown().await;
    // The persistence tap writes on its own task: wait for both final turns
    // to be on disk (#815) instead of sleeping.
    let persisted = crate::eventually(|| {
        replayed_texts(&store, &cfg, &fork) == 4 && replayed_texts(&store, &cfg, &src) == 6
    })
    .await;
    assert!(persisted, "both logs persisted in time");

    let fork_log = store.read(&fork).unwrap();
    let prompts: Vec<String> = fork_log
        .iter()
        .filter_map(|r| match &r.payload {
            LogPayload::In(InMsg::Prompt { content, .. }) => Some(content_text(content)),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, ["three"], "no blank seed prompt is logged");
    let replayed = Session::replay(&pair_records(&fork_log), &cfg, &fork).unwrap();
    let fork_texts = texts(&replayed);
    assert_eq!(fork_texts.len(), 4, "{fork_texts:?}");
    assert_eq!(fork_texts[0], "one");
    assert_eq!(fork_texts[2], "three");
    assert!(
        !fork_texts.iter().any(|t| t == "two"),
        "turn 2 was cut: {fork_texts:?}"
    );

    let src_log = store.read(&src).unwrap();
    assert!(!src_log
        .iter()
        .any(|r| matches!(r.payload, LogPayload::Out(OutEvent::SessionForked { .. }))));
    let replayed_src = Session::replay(&pair_records(&src_log), &cfg, &src).unwrap();
    let src_texts = texts(&replayed_src);
    assert_eq!(src_texts.len(), 6, "{src_texts:?}");
    assert!(!src_texts.iter().any(|t| t == "three"));
}

/// How many texts `id`'s on-disk log replays to so far (0 if unreadable).
fn replayed_texts(store: &SessionStore, cfg: &EngineConfig, id: &SessionId) -> usize {
    store
        .read(id)
        .ok()
        .and_then(|log| Session::replay(&pair_records(&log), cfg, id).ok())
        .map_or(0, |s| texts(&s).len())
}

fn logged_prompts(log: &[LogRecord]) -> Vec<String> {
    log.iter()
        .filter_map(|r| match &r.payload {
            LogPayload::In(InMsg::Prompt { content, .. }) => Some(content_text(content)),
            _ => None,
        })
        .collect()
}

/// `heads::start_fresh` (#752) opens a launch with an agent/`--mode` through
/// an empty-prompt `Spawn`: it queues no turn, so the log records no prompt —
/// no phantom blank user message on resume, and `/rewind`'s turn 1 is the
/// first real prompt.
#[tokio::test]
async fn an_empty_spawn_prompt_logs_no_prompt_record() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::at(tmp.path());
    let cfg = EngineConfig::default();
    let holly = Holly::spawn(cfg.clone());
    let _tap = spawn_persistence_subscriber(&holly, store.clone());
    let mut sub = holly.subscribe();
    let sid = SessionId::new("fresh");
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: Some("plan".into()),
            workdir: None,
        })
        .await
        .unwrap();
    until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == sid),
    )
    .await;
    turn(&holly, &mut sub, &sid, "hi").await;
    holly.shutdown().await;
    let persisted = crate::eventually(|| replayed_texts(&store, &cfg, &sid) >= 2).await;
    assert!(persisted, "the turn persisted in time");

    let log = store.read(&sid).unwrap();
    assert_eq!(logged_prompts(&log), ["hi"]);
    let replayed = Session::replay(&pair_records(&log), &cfg, &sid).unwrap();
    assert_eq!(texts(&replayed)[0], "hi", "no phantom blank user message");
}

/// A log written before the fix keeps its blank spawn prompt. Replay still
/// folds it and counts it as turn 1, exactly as `/rewind` numbers the log's
/// `Prompt` records, so the two sides stay consistent on old logs.
#[test]
fn an_old_log_with_a_blank_spawn_prompt_counts_it_on_both_sides() {
    let sid = SessionId::new("old");
    let out = |ev: OutEvent| LogRecord::new(sid.clone(), LogPayload::Out(ev));
    let prompt = |text: &str| {
        LogRecord::new(
            sid.clone(),
            LogPayload::In(InMsg::prompt(sid.clone(), text)),
        )
    };
    let log = vec![
        out(OutEvent::SessionStarted {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        }),
        prompt(""),
        out(OutEvent::Status {
            session: sid.clone(),
            state: entanglement_core::AgentState::Idle,
        }),
        prompt("hi"),
        out(OutEvent::TextDelta {
            session: sid.clone(),
            seq: 1,
            text: "hello".into(),
        }),
        out(OutEvent::Done {
            session: sid.clone(),
            seq: 2,
        }),
    ];
    assert_eq!(logged_prompts(&log), ["", "hi"], "`/rewind` sees two turns");
    let replayed = Session::replay(&pair_records(&log), &EngineConfig::default(), &sid).unwrap();
    assert_eq!(texts(&replayed), ["", "hi", "hello"]);
    let turn_one: Vec<String> = replayed
        .ctx
        .prefix(Some(1))
        .iter()
        .map(|m| content_text(&m.content))
        .collect();
    assert_eq!(turn_one, [""], "turn 1 is the blank prompt on both sides");
}
