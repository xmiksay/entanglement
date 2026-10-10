//! Unknown-session policy edge cases (#721, ADR-0210): what happens to an
//! in-flight `SessionLoader` call when something else touches its id — a
//! close, an explicit `Resume`, or the loader itself panicking. Shares the
//! in-memory loader and helpers from `unknown_session.rs`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{InMsg, OutEvent, SessionId, UnknownSession};
use tokio::sync::Semaphore;

use crate::unknown_session::{
    assert_nothing_spawned, engine, error_text, one_turn_log, recv_until, MapLoader,
};

fn set_name(sid: &SessionId, name: &str) -> InMsg {
    InMsg::SetSessionMeta {
        session: sid.clone(),
        name: Some(name.into()),
        action: None,
        if_unset: false,
    }
}

#[tokio::test]
async fn close_during_an_in_flight_load_abandons_it() {
    let sid = SessionId::new("closing");
    let gate = Arc::new(Semaphore::new(0));
    let loader = Arc::new(MapLoader {
        gate: Some(gate.clone()),
        ..MapLoader::default()
    });
    let log = one_turn_log(&sid).await;
    loader.logs.lock().unwrap().insert(sid.clone(), log);
    let (holly, _) = engine(UnknownSession::Resume(loader));
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
    assert!(error_text(ev).contains("closed"));
    gate.add_permits(10);
    assert_nothing_spawned(&holly, &mut sub, &sid).await;
}

#[tokio::test]
async fn a_panicking_loader_surfaces_as_an_error_and_spawns_nothing() {
    let loader = Arc::new(MapLoader {
        panic: true,
        ..MapLoader::default()
    });
    let (holly, _) = engine(UnknownSession::Resume(loader));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("kaboom");

    holly.send(InMsg::prompt(sid.clone(), "hi")).await.unwrap();
    holly.send(set_name(&sid, "queued")).await.unwrap();
    // Both the triggering and the queued message are refused, one Error each.
    for _ in 0..2 {
        let ev = recv_until(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
        let text = error_text(ev);
        assert!(
            text.contains("kaboom") && text.contains("panicked"),
            "{text}"
        );
    }
    assert_nothing_spawned(&holly, &mut sub, &sid).await;
}

#[tokio::test]
async fn an_explicit_resume_racing_a_load_keeps_queueing_and_never_resumes_twice() {
    let sid = SessionId::new("raced");
    let gate = Arc::new(Semaphore::new(0));
    let loader = Arc::new(MapLoader {
        gate: Some(gate.clone()),
        ..MapLoader::default()
    });
    let log = one_turn_log(&sid).await;
    loader.logs.lock().unwrap().insert(sid.clone(), log.clone());
    let (holly, _) = engine(UnknownSession::Resume(loader.clone()));
    let mut sub = holly.subscribe();
    let started = |e: &OutEvent| matches!(e, OutEvent::SessionStarted { session, .. } if session.0 == "raced");

    holly.send(set_name(&sid, "a")).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    recv_until(&mut sub, started).await;
    assert!(holly.is_live(&sid), "the explicit Resume made it live");

    // Live now, but its queue is still draining behind the load: this must
    // queue behind "a", not overtake it.
    holly.send(set_name(&sid, "b")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.add_permits(10);

    let mut names = Vec::new();
    while names.len() < 2 {
        let ev = recv_until(&mut sub, |e| {
            started(e) || matches!(e, OutEvent::SessionMetaChanged { .. })
        })
        .await;
        match ev {
            OutEvent::SessionMetaChanged { name, .. } => names.extend(name),
            other => panic!("a second resume must not happen: {other:?}"),
        }
    }
    assert_eq!(names, ["a", "b"], "queued frames deliver in arrival order");
    assert_eq!(loader.calls.load(Ordering::SeqCst), 1);
    let again =
        tokio::time::timeout(Duration::from_millis(200), recv_until(&mut sub, started)).await;
    assert!(
        again.is_err(),
        "the loaded records are discarded, not resumed"
    );
}
