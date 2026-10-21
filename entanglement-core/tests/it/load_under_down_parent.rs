//! An ADR-0210 load of a sub-agent that finishes after an ancestor went down
//! (#905, ADR-0246): hibernated while the load was in flight, or closed, the
//! child does not start and each queued send is refused once.

use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId, UnknownSession};
use tokio::sync::{broadcast::Receiver, Semaphore};

use super::resume_mode::{done, engine_with, hibernated, is, tree, Recorder};
use super::seq_robustness::config;
use super::unknown_session::{MapLoader, Records};

/// A root log whose child hibernated on its own, so resuming the root
/// leaves the child down.
async fn log_with_hibernated_child(root: &SessionId, child: &SessionId) -> Records {
    let holly = engine_with(UnknownSession::SpawnBlank);
    let mut rec = Recorder::new(&holly, &[root, child]);
    tree(&holly, &mut rec, root, child).await;
    holly.hibernate(child.clone()).await.unwrap();
    rec.until(is(child, hibernated)).await;
    rec.log
}

/// Resume `root` live, then queue two sends for `child` behind a gated load.
async fn load_child_behind_gate() -> (
    Holly,
    Receiver<OutEvent>,
    SessionId,
    SessionId,
    Arc<Semaphore>,
    Records,
) {
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let log = log_with_hibernated_child(&root, &child).await;
    let gate = Arc::new(Semaphore::new(0));
    let loader = MapLoader {
        gate: Some(gate.clone()),
        ..MapLoader::default()
    };
    loader
        .logs
        .lock()
        .unwrap()
        .insert(child.clone(), log.clone());
    let holly = engine_with(UnknownSession::Resume(Arc::new(loader)));
    let mut sub = holly.subscribe();
    holly.resume(root.clone(), log.clone()).await.unwrap();
    while !matches!(sub.recv().await, Ok(OutEvent::SessionStarted { session, .. }) if session == root)
    {
    }
    for text in ["one", "two"] {
        holly
            .send(InMsg::prompt(child.clone(), text))
            .await
            .unwrap();
    }
    (holly, sub, root, child, gate, log)
}

/// Count `child`'s errors naming `needle` until the stream goes quiet; a
/// start of the child fails the test.
async fn refusals(sub: &mut Receiver<OutEvent>, child: &SessionId, needle: &str) -> usize {
    let mut count = 0;
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted { session, .. }) if &session == child => {
                    panic!("the child started under a down ancestor")
                }
                Ok(OutEvent::Error {
                    session, message, ..
                }) if &session == child => {
                    assert!(message.contains(needle), "{message}");
                    count += 1;
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    })
    .await;
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_load_finishing_under_a_hibernated_ancestor_is_refused() {
    let (holly, mut sub, root, child, gate, _) = load_child_behind_gate().await;
    holly.hibernate(root.clone()).await.unwrap();
    while !matches!(sub.recv().await, Ok(OutEvent::SessionHibernated { session, .. }) if session == root)
    {
    }
    gate.add_permits(1);
    assert_eq!(refusals(&mut sub, &child, "hibernated").await, 2);
    assert!(!holly.is_live(&child));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_load_finishing_under_a_closed_ancestor_is_refused() {
    let (holly, mut sub, root, child, gate, _) = load_child_behind_gate().await;
    holly
        .send(InMsg::CloseSession {
            session: root.clone(),
        })
        .await
        .unwrap();
    while !matches!(sub.recv().await, Ok(OutEvent::SessionEnded { session, .. }) if session == root)
    {
    }
    gate.add_permits(1);
    assert_eq!(refusals(&mut sub, &child, "ancestor").await, 2);
    assert!(!holly.is_live(&child));
}

/// The loading id itself hibernated while it loaded: refused the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_load_hibernated_while_in_flight_is_refused() {
    let (holly, mut sub, _root, child, gate, _) = load_child_behind_gate().await;
    holly.hibernate(child.clone()).await.unwrap();
    // The hibernate is handled before the load completes: the supervisor
    // takes inbox frames in order, and the gate is still shut.
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.add_permits(1);
    let needle = "this session was hibernated";
    assert_eq!(refusals(&mut sub, &child, needle).await, 2);
    assert!(!holly.is_live(&child));
}

/// An ancestor hibernated during the load but resumed again before it
/// finished is live: the child starts and gets its queued sends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_load_under_a_revived_ancestor_starts() {
    let (holly, mut sub, root, child, gate, log) = load_child_behind_gate().await;
    holly.hibernate(root.clone()).await.unwrap();
    holly.resume(root.clone(), log).await.unwrap();
    while !matches!(sub.recv().await, Ok(OutEvent::SessionStarted { session, .. }) if session == root)
    {
    }
    gate.add_permits(1);
    let mut done = 0;
    while done < 2 {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await {
            Ok(Ok(OutEvent::Error {
                session, message, ..
            })) if session == child => {
                panic!("refused under a live ancestor: {message}")
            }
            Ok(Ok(OutEvent::Done { session, .. })) if session == child => done += 1,
            Ok(_) => {}
            Err(_) => panic!("the child never ran its queued sends"),
        }
    }
}

/// A sub-tree member with a load in flight is about to run: its root is not
/// idle-hibernated meanwhile.
#[tokio::test(start_paused = true)]
async fn a_loading_child_pins_its_root_against_the_idle_sweep() {
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let loader = MapLoader {
        gate: Some(Arc::new(Semaphore::new(0))),
        ..MapLoader::default()
    };
    let holly = Holly::spawn(EngineConfig {
        idle_ttl: Some(Duration::from_secs(120)),
        unknown_session: UnknownSession::Resume(Arc::new(loader)),
        ..config(1, Duration::ZERO)
    });
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    spawned_tree(&holly, &mut rec, &root, &child).await;
    holly.hibernate(child.clone()).await.unwrap();
    rec.until(is(&child, hibernated)).await;
    holly
        .send(InMsg::prompt(child.clone(), "hi"))
        .await
        .unwrap();

    let evicted = tokio::time::timeout(Duration::from_secs(3600), async {
        loop {
            if let Ok(OutEvent::SessionHibernated { session, .. }) = rec.sub.recv().await {
                if session == root {
                    return;
                }
            }
        }
    })
    .await;
    assert!(
        evicted.is_err(),
        "the root was idle-hibernated under a loading child"
    );
}

/// Under `UnknownSession::Resume` a prompt to an unknown root would load it,
/// so the tree is spawned explicitly.
async fn spawned_tree(holly: &Holly, rec: &mut Recorder, root: &SessionId, child: &SessionId) {
    for (session, parent) in [(root, None), (child, Some(root.clone()))] {
        let spawn = InMsg::Spawn {
            session: session.clone(),
            parent,
            predecessor: None,
            agent: "general".into(),
            prompt: "task".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        };
        holly.send(spawn).await.unwrap();
        rec.until(is(session, done)).await;
    }
}

/// In one engine a hibernated child keeps its `parent_links` entry, so its
/// root's hibernate reaches the loading child too. That counts as an
/// ancestor going down, not the child's own hibernate: the root resumed
/// before the load finished, so the child starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_load_under_a_root_revived_in_process_starts() {
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let gate = Arc::new(Semaphore::new(0));
    let loader = Arc::new(MapLoader {
        gate: Some(gate.clone()),
        ..MapLoader::default()
    });
    let holly = engine_with(UnknownSession::Resume(loader.clone()));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    spawned_tree(&holly, &mut rec, &root, &child).await;
    holly.hibernate(child.clone()).await.unwrap();
    rec.until(is(&child, hibernated)).await;
    let log = rec.log.clone();
    loader
        .logs
        .lock()
        .unwrap()
        .insert(child.clone(), log.clone());
    holly
        .send(InMsg::prompt(child.clone(), "one"))
        .await
        .unwrap();
    holly.hibernate(root.clone()).await.unwrap();
    rec.until(is(&root, hibernated)).await;
    holly.resume(root.clone(), log).await.unwrap();
    rec.until(|e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == root))
        .await;
    gate.add_permits(1);
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rec.sub.recv())
            .await
            .expect("the child never ran its queued send")
            .expect("event stream open");
        match ev {
            OutEvent::Error {
                session, message, ..
            } if session == child => panic!("refused under a live root: {message}"),
            OutEvent::Done { session, .. } if session == child => break,
            _ => {}
        }
    }
}
