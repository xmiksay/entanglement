//! A spawned child's initiating task prompt must be reconstructible on replay
//! (#421). `InMsg::Spawn` delivers the prompt straight to the child's
//! session-command channel, bypassing the inbound broadcast the persistence
//! tap observes — without the synthesized `InMsg::Prompt` this test asserts
//! for, `Session::replay` would fold the assistant's eventual reply but never
//! the user-role instruction that produced it.

use std::time::Duration;

use entanglement_core::{
    content_text, session::Session, EngineConfig, Holly, InMsg, MessageRole, OutEvent, SessionId,
};
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::session_store::{pair_records, read, LogPayload};

#[tokio::test]
async fn spawned_child_prompt_is_persisted_and_replayable() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let cwd = tmp.path().to_path_buf();

    let holly = Holly::spawn(EngineConfig::default());
    let _tap = spawn_persistence_subscriber(&holly, cwd.clone());
    let mut sub = holly.subscribe();

    let child = SessionId::new("child");
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: None,
            predecessor: None,
            agent: "build".into(),
            prompt: "child task".into(),
        })
        .await
        .expect("send spawn");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("timed out waiting for child Done")
            .expect("broadcast closed");
        if ev.session() == Some(&child) && matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }

    // The tap writes concurrently off its own subscription; wait until it has
    // flushed the turn's Done before reading the file back.
    let flushed = tokio::time::Instant::now() + Duration::from_secs(5);
    let records = loop {
        let records = read(&cwd, &child).expect("read log");
        if records
            .iter()
            .any(|r| matches!(&r.payload, LogPayload::Out(OutEvent::Done { .. })))
        {
            break records;
        }
        assert!(
            tokio::time::Instant::now() < flushed,
            "tap never flushed the child's Done"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // `InMsg::Spawn` itself must never be persisted verbatim (it would create a
    // stray record; the child's root is only resolvable once `SessionStarted`
    // arrives).
    assert!(
        !records
            .iter()
            .any(|r| matches!(&r.payload, LogPayload::In(InMsg::Spawn { .. }))),
        "InMsg::Spawn must not be persisted verbatim"
    );

    // A synthesized `InMsg::Prompt` carrying the spawn prompt must exist,
    // attributed to the child session.
    let synthesized = records
        .iter()
        .find(|r| {
            matches!(
                &r.payload,
                LogPayload::In(InMsg::Prompt { session, .. }) if *session == child
            )
        })
        .expect("synthesized spawn-prompt record must exist");
    let LogPayload::In(InMsg::Prompt { content, .. }) = &synthesized.payload else {
        unreachable!();
    };
    assert_eq!(content_text(content), "child task");

    // Exactly one such record — a resumed-then-replayed cycle must not
    // duplicate it.
    let count = records
        .iter()
        .filter(|r| matches!(&r.payload, LogPayload::In(InMsg::Prompt { session, .. }) if *session == child))
        .count();
    assert_eq!(
        count, 1,
        "the spawn prompt must be synthesized exactly once"
    );

    // Replay must reconstruct the child's `Context` with the spawn prompt as
    // its opening user message, not just the assistant's eventual reply.
    let cfg = EngineConfig::default();
    let paired = pair_records(&records);
    let session = Session::replay(&paired, &cfg, &child).expect("replay");
    let messages = session.ctx.messages();
    assert!(
        !messages.is_empty(),
        "replayed context must not be empty (the spawn prompt was dropped)"
    );
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].text(), "child task");
}
