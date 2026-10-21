//! One task per id (#892, ADR-0244): a task started while the id's old task
//! tears down waits for it, buffers what it is sent in arrival order, and is
//! an ordinary live session to every cascade meanwhile.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::Receiver;

use super::seq_robustness::{
    assert_b_starts_after_a_exits, config, next_for, record_turn, slow_reverter,
};

pub(super) fn engine() -> Holly {
    Holly::spawn(EngineConfig {
        file_reverter: Some(slow_reverter()),
        ..config(1, Duration::ZERO)
    })
}

pub(super) async fn start_slow_revert(holly: &Holly, sid: &SessionId) {
    holly
        .send(InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Spawn `child` under `root`, run its first turn, and append its events to
/// the root's resume `log`.
pub(super) async fn spawn_child(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    root: &SessionId,
    child: &SessionId,
    log: &mut Vec<(Option<InMsg>, OutEvent)>,
) {
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(root.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: "child task".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    // `Spawn` is never persisted; the child's own events land in the root log.
    loop {
        let ev = next_for(sub, child).await;
        let done = matches!(ev, OutEvent::Done { .. });
        log.push((None, ev));
        if done {
            break;
        }
    }
}

/// The root's hibernate cascades onto a child that is mid-revert, so the
/// child's old task is waited for too: an explicit resume of the child, sent
/// right after the root's, starts it only after that task exited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_hibernate_waits_for_its_live_child_too() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut log = record_turn(&holly, &mut sub, &root).await;
    spawn_child(&holly, &mut sub, &root, &child, &mut log).await;

    start_slow_revert(&holly, &child).await;
    holly.hibernate(root.clone()).await.unwrap();
    while !matches!(
        next_for(&mut sub, &root).await,
        OutEvent::SessionHibernated { .. }
    ) {}
    holly.resume(root.clone(), log.clone()).await.unwrap();
    holly.resume(child.clone(), log).await.unwrap();
    assert_b_starts_after_a_exits(&mut sub, &child).await;
    assert!(holly.is_live(&root) && holly.is_live(&child));
}

/// A resume cascade that reaches a child still unwinding from its own
/// hibernate leaves it hibernated: the records lack only its coming
/// `SessionHibernated`, and the complete log would not revive it either. A
/// prompt for it, sent while it tears down, is refused like any prompt to a
/// hibernated sub-agent child, and nothing revives the child afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_cascade_leaves_a_still_retiring_child_hibernated() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut log = record_turn(&holly, &mut sub, &root).await;
    spawn_child(&holly, &mut sub, &root, &child, &mut log).await;

    start_slow_revert(&holly, &child).await;
    holly.hibernate(child.clone()).await.unwrap();
    holly.hibernate(root.clone()).await.unwrap();
    while !matches!(
        next_for(&mut sub, &root).await,
        OutEvent::SessionHibernated { .. }
    ) {}
    holly.resume(root.clone(), log).await.unwrap();
    holly
        .send(InMsg::prompt(child.clone(), "hi"))
        .await
        .unwrap();

    let (mut refused, mut hibernated) = (false, false);
    let _ = tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            match next_for(&mut sub, &child).await {
                OutEvent::SessionStarted { .. } => panic!("the cascade revived the child"),
                OutEvent::SessionHibernated { .. } => hibernated = true,
                OutEvent::Error { .. } => refused = true,
                _ => {}
            }
        }
    })
    .await;
    assert!(
        hibernated && refused,
        "hibernated {hibernated}, refused {refused}"
    );
    assert!(holly.is_live(&root));
    assert!(!holly.is_live(&child));
}

/// A resume and then a prompt, both sent while the old task is still
/// reverting, are handled in that order: the resume starts the new task,
/// which waits for the old one, and the prompt buffers in its inbox and runs
/// a turn once it started. Reversed, the prompt would lazily spawn a blank
/// session and the resume would be refused as already live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_then_a_prompt_behind_a_teardown_keep_their_order() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("order");
    let log = record_turn(&holly, &mut sub, &sid).await;

    start_slow_revert(&holly, &sid).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();

    let (mut started, mut hibernated) = (0, false);
    loop {
        match next_for(&mut sub, &sid).await {
            OutEvent::SessionHibernated { .. } => hibernated = true,
            OutEvent::SessionStarted { .. } => {
                assert!(hibernated, "B started before A exited");
                started += 1;
            }
            OutEvent::Error { message, .. } => panic!("unexpected error: {message}"),
            OutEvent::Done { .. } => break,
            _ => {}
        }
    }
    let late = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            let ev = next_for(&mut sub, &sid).await;
            if matches!(ev, OutEvent::SessionStarted { .. } | OutEvent::Error { .. }) {
                return ev;
            }
        }
    })
    .await;
    assert!(late.is_err(), "late event: {late:?}");
    assert_eq!(started, 1, "exactly one task started");
}

/// Hibernate `child` alone behind a slow revert, then resume it explicitly:
/// the new task waits for the old one to exit.
pub(super) async fn child_resume_behind_teardown(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    root: &SessionId,
    child: &SessionId,
) {
    let mut log = record_turn(holly, sub, root).await;
    spawn_child(holly, sub, root, child, &mut log).await;
    start_slow_revert(holly, child).await;
    holly.hibernate(child.clone()).await.unwrap();
    holly.resume(child.clone(), log).await.unwrap();
}

/// A root hibernate cascades onto a child whose new task is still waiting
/// for its old one: the waiting task is a live session to the cascade, so it
/// is hibernated too, and, hibernated before it ever ran, it never starts —
/// no child live under a hibernated root.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_hibernate_reaches_a_child_resumed_behind_its_teardown() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    child_resume_behind_teardown(&holly, &mut sub, &root, &child).await;
    holly.hibernate(root.clone()).await.unwrap();

    while !matches!(
        next_for(&mut sub, &child).await,
        OutEvent::SessionHibernated { .. }
    ) {}
    let late = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if let ev @ OutEvent::SessionStarted { .. } = next_for(&mut sub, &child).await {
                return ev;
            }
        }
    })
    .await;
    assert!(late.is_err(), "the child started under a hibernated root");
    assert!(!holly.is_live(&root));
    assert!(!holly.is_live(&child));
}

/// A root `SetMode` cascades onto a child whose new task is still waiting
/// for its old one: the switch buffers in the waiting task's inbox, so the
/// child ends in the root's new mode, not the one it had before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_set_mode_reaches_a_child_resumed_behind_its_teardown() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    child_resume_behind_teardown(&holly, &mut sub, &root, &child).await;
    holly
        .send(InMsg::SetMode {
            session: root.clone(),
            mode: "research".into(),
        })
        .await
        .unwrap();

    while !matches!(
        next_for(&mut sub, &child).await,
        OutEvent::SessionStarted { .. }
    ) {}
    let switched = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let OutEvent::ModeChanged { mode, .. } = next_for(&mut sub, &child).await {
                if mode == "research" {
                    return;
                }
            }
        }
    })
    .await;
    assert!(switched.is_ok(), "the resumed child kept its old mode");
}
