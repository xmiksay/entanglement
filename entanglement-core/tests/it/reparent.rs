//! Live sub-agents survive compaction (ADR-0234): the supervisor moves a
//! compacting session's live children under its successor before the
//! source's close cascade runs, places a sub-agent's successor under its
//! parent, and leaves closed children and `/fork` copies alone.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::Receiver;

use super::compaction_fork::{prune_fork, prune_only_engine};

const WAIT: Duration = Duration::from_secs(5);

fn spawn(
    session: &SessionId,
    parent: Option<&SessionId>,
    predecessor: Option<&SessionId>,
) -> InMsg {
    InMsg::Spawn {
        session: session.clone(),
        parent: parent.cloned(),
        predecessor: predecessor.cloned(),
        agent: "general".into(),
        // Empty: no turn runs, the session just stays live.
        prompt: String::new(),
        user: None,
        model: None,
        mode: None,
        workdir: None,
    }
}

/// Collect every event up to and including the first matching `stop`.
async fn until(sub: &mut Receiver<OutEvent>, stop: impl Fn(&OutEvent) -> bool) -> Vec<OutEvent> {
    let mut seen = Vec::new();
    loop {
        let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await else {
            panic!("stop event never arrived; saw {seen:?}");
        };
        let done = stop(&ev);
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

fn ended(id: &SessionId) -> impl Fn(&OutEvent) -> bool + '_ {
    move |ev| matches!(ev, OutEvent::SessionEnded { session, .. } if session == id)
}

fn started(id: &SessionId) -> impl Fn(&OutEvent) -> bool + '_ {
    move |ev| matches!(ev, OutEvent::SessionStarted { session, .. } if session == id)
}

/// Start `ids` in order (each a child of the one given, or a root) and wait
/// until all have announced themselves.
async fn start(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    ids: &[(&SessionId, Option<&SessionId>)],
) {
    for (id, parent) in ids {
        holly.send(spawn(id, *parent, None)).await.unwrap();
        until(sub, started(id)).await;
    }
}

/// The exact pair a compaction fork sends (`fork.rs`), minus the in-process
/// inheritance that only the fork channel carries.
async fn compact(holly: &Holly, source: &SessionId, successor: &SessionId) {
    holly
        .send(spawn(successor, None, Some(source)))
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: source.clone(),
        })
        .await
        .unwrap();
}

/// A real compaction fork (the prune fallback): the source's live child is
/// re-linked to the successor, announced with `ParentChanged`, and survives
/// the source's close — then dies with the successor's, proving the link.
#[tokio::test]
async fn a_real_compaction_moves_the_live_child_to_the_successor() {
    let holly = prune_only_engine();
    let mut sub = holly.subscribe();
    let (sid, child) = (SessionId::new("s1"), SessionId::new("s1-child"));
    start(&holly, &mut sub, &[(&sid, None), (&child, Some(&sid))]).await;

    // Its own receiver: the move can land before the successor's
    // `SessionStarted`, which `prune_fork` reads up to. (The retired source's
    // own `SessionEnded` can precede both: its task ends itself on forking.)
    let mut watch = holly.subscribe();
    let (_events, successor, mut sub) = prune_fork(&holly, &sid).await;
    let seen = until(
        &mut watch,
        |e| matches!(e, OutEvent::ParentChanged { session, .. } if session == &child),
    )
    .await;
    assert!(
        matches!(seen.last(), Some(OutEvent::ParentChanged { parent, .. }) if parent == &successor),
        "the move is wire-visible and names the successor: {seen:?}"
    );

    // Once the supervisor has processed the source's `CloseSession`, a
    // cascade would have dropped the child from the directory synchronously.
    tokio::time::timeout(WAIT, async {
        while holly.session_info(&sid).is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the source closed");
    let info = holly
        .session_info(&child)
        .expect("the child survived the close");
    assert_eq!(info.parent, Some(successor.clone()));
    assert!(holly.is_live(&child));
    assert_eq!(holly.successor_of(&sid), Some(successor.clone()));

    holly
        .send(InMsg::CloseSession {
            session: successor.clone(),
        })
        .await
        .unwrap();
    until(&mut sub, ended(&child)).await;
}

/// Only live children move: one closed beforehand stays out of the
/// directory, and no `ParentChanged` names it.
#[tokio::test]
async fn an_already_closed_child_is_left_alone() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let (pred, succ) = (SessionId::new("cl-pred"), SessionId::new("cl-succ"));
    let (gone, kept) = (SessionId::new("cl-gone"), SessionId::new("cl-kept"));
    start(
        &holly,
        &mut sub,
        &[(&pred, None), (&gone, Some(&pred)), (&kept, Some(&pred))],
    )
    .await;
    holly
        .send(InMsg::CloseSession {
            session: gone.clone(),
        })
        .await
        .unwrap();
    until(&mut sub, ended(&gone)).await;

    compact(&holly, &pred, &succ).await;
    let seen = until(&mut sub, ended(&pred)).await;

    let moved: Vec<&SessionId> = seen
        .iter()
        .filter_map(|e| match e {
            OutEvent::ParentChanged { session, .. } => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(moved, vec![&kept]);
    assert!(holly.session_info(&gone).is_none());
    assert_eq!(holly.session_info(&kept).and_then(|i| i.parent), Some(succ));
}

/// A sub-agent that compacts keeps its place: its successor starts under the
/// same parent (not as a root), and closing that parent cascades onto it.
#[tokio::test]
async fn a_compacting_sub_agents_successor_starts_under_its_parent() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let (root, child, child_succ) = (
        SessionId::new("sa-root"),
        SessionId::new("sa-child"),
        SessionId::new("sa-child-succ"),
    );
    start(&holly, &mut sub, &[(&root, None), (&child, Some(&root))]).await;

    compact(&holly, &child, &child_succ).await;
    let seen = until(&mut sub, started(&child_succ)).await;
    match seen.last() {
        Some(OutEvent::SessionStarted {
            parent,
            predecessor,
            root: is_root,
            ..
        }) => {
            assert_eq!(parent.as_ref(), Some(&root));
            assert_eq!(predecessor.as_ref(), Some(&child));
            assert!(!is_root);
        }
        other => panic!("expected the successor's SessionStarted, got {other:?}"),
    }
    until(&mut sub, ended(&child)).await;
    assert!(holly.is_live(&root), "the parent is untouched");

    holly
        .send(InMsg::CloseSession {
            session: root.clone(),
        })
        .await
        .unwrap();
    until(&mut sub, ended(&child_succ)).await;
}

/// A `Spawn` that names a parent which compacted away (it raced the fork)
/// joins the successor rather than a closed id.
#[tokio::test]
async fn a_spawn_naming_a_compacted_parent_joins_its_successor() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let (pred, succ, late) = (
        SessionId::new("rc-pred"),
        SessionId::new("rc-succ"),
        SessionId::new("rc-late"),
    );
    start(&holly, &mut sub, &[(&pred, None)]).await;
    compact(&holly, &pred, &succ).await;
    until(&mut sub, ended(&pred)).await;

    holly.send(spawn(&late, Some(&pred), None)).await.unwrap();
    let seen = until(&mut sub, started(&late)).await;
    assert!(
        matches!(seen.last(), Some(OutEvent::SessionStarted { parent: Some(p), .. }) if p == &succ),
        "{seen:?}"
    );
}

/// `/fork` (ADR-0223) copies without retiring the source: its children stay
/// where they are.
#[tokio::test]
async fn a_user_fork_moves_no_children() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let (src, child) = (SessionId::new("uf-src"), SessionId::new("uf-child"));
    start(&holly, &mut sub, &[(&src, None), (&child, Some(&src))]).await;

    holly
        .send(InMsg::ForkSession {
            session: src.clone(),
            at_turn: None,
        })
        .await
        .unwrap();
    let seen = until(&mut sub, |e| matches!(e, OutEvent::SessionForked { .. })).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, OutEvent::ParentChanged { .. })),
        "{seen:?}"
    );
    assert_eq!(
        holly.session_info(&child).and_then(|i| i.parent),
        Some(src.clone())
    );
    assert!(holly.is_live(&src));
}

/// A `Spawn` naming the source that lands between the fork's `Spawn` and its
/// `CloseSession` joins the successor instead of dying in the cascade.
#[tokio::test]
async fn a_spawn_racing_the_forks_close_joins_the_successor() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let (pred, succ, late) = (
        SessionId::new("rw-pred"),
        SessionId::new("rw-succ"),
        SessionId::new("rw-late"),
    );
    start(&holly, &mut sub, &[(&pred, None)]).await;
    holly.send(spawn(&succ, None, Some(&pred))).await.unwrap();
    holly.send(spawn(&late, Some(&pred), None)).await.unwrap();
    holly
        .send(InMsg::CloseSession {
            session: pred.clone(),
        })
        .await
        .unwrap();
    let seen = until(&mut sub, ended(&pred)).await;
    assert!(!seen.iter().any(ended(&late)), "{seen:?}");
    assert_eq!(
        holly.session_info(&late).and_then(|i| i.parent),
        Some(succ.clone())
    );
}

/// Only direct children move; a grandchild stays under its own parent and
/// survives with it.
#[tokio::test]
async fn a_grandchild_stays_under_its_parent_across_the_compaction() {
    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    let (pred, succ, kid, grandkid) = (
        SessionId::new("gc-pred"),
        SessionId::new("gc-succ"),
        SessionId::new("gc-kid"),
        SessionId::new("gc-grandkid"),
    );
    start(
        &holly,
        &mut sub,
        &[(&pred, None), (&kid, Some(&pred)), (&grandkid, Some(&kid))],
    )
    .await;
    compact(&holly, &pred, &succ).await;
    let seen = until(&mut sub, ended(&pred)).await;
    let moved: Vec<&SessionId> = seen
        .iter()
        .filter_map(|e| match e {
            OutEvent::ParentChanged { session, .. } => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(moved, vec![&kid]);
    assert_eq!(
        holly.session_info(&grandkid).and_then(|i| i.parent),
        Some(kid.clone())
    );
    assert_eq!(holly.session_info(&kid).and_then(|i| i.parent), Some(succ));
}
