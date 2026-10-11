//! `/fork` through the persistence tap (#786, ADR-0223): the fork gets its own
//! root log that replays to exactly the history it started from — the
//! source's turns up to the cut, no blank seed prompt — while the source's log
//! records no fork history and the source keeps running.

use std::time::Duration;

use entanglement_core::{
    content_text, session::Session, EngineConfig, Holly, InMsg, OutEvent, SessionId,
};
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::session_store::{pair_records, LogPayload, SessionStore};
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
    tokio::time::sleep(Duration::from_millis(100)).await;

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
