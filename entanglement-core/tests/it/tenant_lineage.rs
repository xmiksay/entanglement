//! Tenant-prefix inheritance and `Holly::session_info` (#722, ADR-0211): a
//! sub-agent child and a compaction successor of a `{tenant}:` root keep the
//! prefix, and the engine's directory answers who owns a session
//! synchronously.

use std::time::Duration;

use entanglement_core::{mint_id, EngineConfig, Holly, IdKind, InMsg, OutEvent, SessionId, UserId};

use crate::compaction_fork::{prune_fork, prune_only_engine};

async fn started(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>, id: &SessionId) {
    wait_for(
        sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if session == id),
    )
    .await;
}

async fn wait_for(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match sub.recv().await {
                Ok(ev) if pred(&ev) => return ev,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for a matching event")
}

/// Bind `id` to `user` without running a turn (an empty `Spawn` prompt).
async fn spawn_root(holly: &Holly, id: &SessionId, user: Option<UserId>) {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: id.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            prompt: String::new(),
            user,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    started(&mut sub, id).await;
}

#[tokio::test]
async fn a_child_of_a_tenant_root_keeps_its_prefix_and_reports_its_lineage() {
    let holly = Holly::spawn(EngineConfig::default());
    let root = SessionId::new("t1:abc");
    let alice = UserId::new("alice");
    spawn_root(&holly, &root, Some(alice.clone())).await;

    let child = SessionId::new(mint_id(IdKind::Session, Some(&root)));
    assert!(child.0.starts_with("t1:s-"), "child id {child}");

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(root.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: String::new(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    started(&mut sub, &child).await;

    let info = holly.session_info(&child).expect("a live child is listed");
    assert_eq!(info.parent, Some(root.clone()));
    assert_eq!(info.user, Some(alice));
    assert!(!info.root);
    assert_eq!(info.predecessor, None);
}

#[tokio::test]
async fn a_compaction_successor_keeps_the_prefix_predecessor_and_user() {
    let holly = prune_only_engine();
    let root = SessionId::new("t1:abc");
    let alice = UserId::new("alice");
    spawn_root(&holly, &root, Some(alice.clone())).await;

    let (_, successor, _sub) = prune_fork(&holly, &root).await;
    assert!(successor.0.starts_with("t1:s-"), "successor id {successor}");

    let info = holly
        .session_info(&successor)
        .expect("the successor is live once it has started");
    assert_eq!(info.predecessor, Some(root));
    assert_eq!(info.parent, None);
    assert!(info.root);
    assert_eq!(info.user, Some(alice));
}

#[tokio::test]
async fn an_unprefixed_root_compacts_into_an_unprefixed_successor() {
    let holly = prune_only_engine();
    let root = SessionId::new("plain-root");
    spawn_root(&holly, &root, None).await;

    let (_, successor, _sub) = prune_fork(&holly, &root).await;
    assert!(
        successor.0.starts_with("s-") && successor.0.len() == 15,
        "successor id {successor}"
    );
}

#[tokio::test]
async fn session_info_is_live_only_and_survives_a_hibernate_resume_cycle() {
    let holly = Holly::spawn(EngineConfig::default());
    assert_eq!(holly.session_info(&SessionId::new("t1:nobody")), None);

    let source = SessionId::new("t1:source");
    let id = SessionId::new("t1:successor");
    let alice = UserId::new("alice");
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: id.clone(),
            parent: None,
            predecessor: Some(source.clone()),
            agent: "general".into(),
            prompt: String::new(),
            user: Some(alice.clone()),
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    let announced = wait_for(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == id),
    )
    .await;
    let before = holly.session_info(&id).expect("live after SessionStarted");
    assert_eq!(before.predecessor, Some(source));
    assert_eq!(before.user, Some(alice));

    holly.hibernate(id.clone()).await.unwrap();
    wait_for(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == id),
    )
    .await;
    assert_eq!(holly.session_info(&id), None, "hibernated ⇒ not listed");

    holly
        .resume(id.clone(), vec![(None, announced)])
        .await
        .unwrap();
    started(&mut sub, &id).await;
    assert_eq!(
        holly.session_info(&id),
        Some(before),
        "resume restores the same lineage and user from the log"
    );
}
