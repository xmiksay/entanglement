//! Every resume path restores the whole tree (ADR-0247): `resume_with_mode`
//! — the TUI and `run --resume` — brings back the sub-agents the root's
//! hibernate put down, after a restart too, but never a closed one.

use std::time::Duration;

use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::attendance::Attendance;
use entanglement_runtime::persistence::spawn_persistence_subscriber;
use entanglement_runtime::session_store::{resume_with_mode, LogPayload, SessionStore};

use crate::attended_mode::{approve, card, ran};
use crate::attended_rejoin::{engine, next_of};
use crate::unattended_mode::collect;

/// On an engine logging to `store`: a root with a sub-agent whose bash call
/// parked a card, the tree then hibernated by the root and logged. Returns
/// the engine (keep it, and its watch, alive) and the card's request id.
async fn hibernated_tree(
    store: &SessionStore,
    root: &SessionId,
    child: &SessionId,
) -> (Holly, Attendance, String) {
    let attendance = Attendance::new();
    let holly = engine(attendance.clone());
    let _tap = spawn_persistence_subscriber(&holly, store.clone());
    let watching = attendance.watch_all();
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::SetMode {
            session: root.clone(),
            mode: "build".into(),
        })
        .await
        .unwrap();
    while !matches!(next_of(&mut sub, root).await, OutEvent::ModeChanged { .. }) {}
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(root.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: "go".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    let rid = card(&mut sub, child).await;
    drop(watching);
    holly.hibernate(root.clone()).await.unwrap();
    for id in [root, child] {
        logged(store, root, |p| {
            matches!(p, LogPayload::Out(OutEvent::SessionHibernated { session, .. }) if session == id)
        })
        .await;
    }
    (holly, attendance, rid)
}

/// Wait until `root`'s log holds a record `hit` accepts.
async fn logged(store: &SessionStore, root: &SessionId, hit: impl Fn(&LogPayload) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !store
        .read(root)
        .unwrap_or_default()
        .iter()
        .any(|r| hit(&r.payload))
    {
        assert!(tokio::time::Instant::now() < deadline, "never logged");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A restart: a fresh engine (a relaunched TUI, watching every session)
/// resumes `root` the TUI/`run --resume` way.
async fn relaunch(
    store: &SessionStore,
    root: &SessionId,
) -> (Holly, entanglement_runtime::attendance::Watcher) {
    let attendance = Attendance::new();
    let watching = attendance.watch_all();
    let holly = engine(attendance);
    let (records, check) = store.load_for_resume(root, |_| true).expect("loads");
    resume_with_mode(&holly, root, &records, &check, false, None)
        .await
        .expect("resumes");
    (holly, watching)
}

/// The sub-agent comes back with its root after a restart, and its parked
/// card is re-offered and answerable.
#[tokio::test]
async fn a_resumed_root_brings_back_its_sub_agents_card() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::at(tmp.path());
    let (root, child) = (SessionId::new("root"), SessionId::new("child"));
    let (_first, _, rid) = hibernated_tree(&store, &root, &child).await;

    // Subscribed before the relaunch resumes, so the re-offer is seen.
    let attendance = Attendance::new();
    let _relaunched = attendance.watch_all();
    let holly = engine(attendance);
    let mut sub = holly.subscribe();
    let (records, check) = store.load_for_resume(&root, |_| true).expect("loads");
    resume_with_mode(&holly, &root, &records, &check, false, None)
        .await
        .expect("resumes");
    let reoffered = card(&mut sub, &child).await;
    assert_eq!(reoffered, rid, "the child's parked card is re-offered");
    let after = holly.subscribe();
    holly.send(approve(&child, &rid)).await.unwrap();
    let events = collect(after, &child).await;
    assert!(ran(&events, "echo hi"), "{events:?}");
}

/// Review F1: a sub-agent closed while hibernated logs only its
/// `CloseSession`; after a restart it stays closed.
#[tokio::test]
async fn a_child_closed_while_hibernated_stays_closed_after_a_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::at(tmp.path());
    let (root, child) = (SessionId::new("root"), SessionId::new("child"));
    let (first, _, _) = hibernated_tree(&store, &root, &child).await;
    first
        .send(InMsg::CloseSession {
            session: child.clone(),
        })
        .await
        .unwrap();
    logged(
        &store,
        &root,
        |p| matches!(p, LogPayload::In(InMsg::CloseSession { session }) if *session == child),
    )
    .await;

    let (holly, _) = relaunch(&store, &root).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(holly.is_live(&root));
    assert!(!holly.is_live(&child), "the closed child came back");
}
