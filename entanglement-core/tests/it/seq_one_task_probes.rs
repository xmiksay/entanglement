//! Regression probes for one task per id (#892, ADR-0244): cascades,
//! closes, refusals and compaction around a task that waits for the id's old
//! task to exit.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{
    EngineConfig, FileReverter, Holly, InMsg, OutEvent, SessionId, UnknownSession,
};
use tokio::sync::broadcast::Receiver;

use super::seq_one_task::{child_resume_behind_teardown, engine, spawn_child, start_slow_revert};
use super::seq_robustness::{
    assert_b_starts_after_a_exits, config, next_for, record_turn, slow_reverter,
};
use super::unknown_session::MapLoader;

/// Every event for `sid` over `window`.
async fn collect(sub: &mut Receiver<OutEvent>, sid: &SessionId, window: Duration) -> Vec<OutEvent> {
    let mut seen = Vec::new();
    let _ = tokio::time::timeout(window, async {
        loop {
            seen.push(next_for(sub, sid).await);
        }
    })
    .await;
    seen
}

fn set_mode(session: &SessionId, mode: &str) -> InMsg {
    InMsg::SetMode {
        session: session.clone(),
        mode: mode.into(),
    }
}

/// R → C → G. C hibernates behind a slow teardown, cascading onto G. G is
/// resumed, then C (whose new task waits), then `SetMode{R, auto}` cascades
/// over both, then `SetMode{G, research}` narrows G. When C's new task starts
/// it applies only its own switch, so G ends in research.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_later_narrowing_of_a_grandchild_survives_its_parents_late_start() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let (r, c, g) = (
        SessionId::new("r"),
        SessionId::new("c"),
        SessionId::new("g"),
    );
    let mut log = record_turn(&holly, &mut sub, &r).await;
    spawn_child(&holly, &mut sub, &r, &c, &mut log).await;
    spawn_child(&holly, &mut sub, &c, &g, &mut log).await;

    start_slow_revert(&holly, &c).await;
    holly.hibernate(c.clone()).await.unwrap();
    while !matches!(
        next_for(&mut sub, &g).await,
        OutEvent::SessionHibernated { .. }
    ) {}
    holly.resume(g.clone(), log.clone()).await.unwrap();
    holly.resume(c.clone(), log).await.unwrap();
    holly.send(set_mode(&r, "auto")).await.unwrap();
    holly.send(set_mode(&g, "research")).await.unwrap();

    let modes: Vec<String> = collect(&mut sub, &g, Duration::from_millis(900))
        .await
        .into_iter()
        .filter_map(|ev| match ev {
            OutEvent::ModeChanged { mode, .. } => Some(mode),
            _ => None,
        })
        .collect();
    assert_eq!(
        modes.last().map(String::as_str),
        Some("research"),
        "{modes:?}"
    );
    assert!(holly.is_live(&c) && holly.is_live(&g));
}

/// kid's new task waits behind its slow teardown; the root is hibernated
/// (cascading onto the waiting task) and then closed. The waiting task never
/// ran, so nothing at all is emitted for it: no start, and no error, since no
/// head send was dropped with a reason owed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_root_leaves_its_waiting_child_silent() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let (root, kid) = (SessionId::new("root"), SessionId::new("kid"));
    child_resume_behind_teardown(&holly, &mut sub, &root, &kid).await;
    holly.hibernate(root.clone()).await.unwrap();
    holly
        .send(InMsg::CloseSession {
            session: root.clone(),
        })
        .await
        .unwrap();

    let seen = collect(&mut sub, &kid, Duration::from_millis(900)).await;
    let hibernated = seen
        .iter()
        .filter(|e| matches!(e, OutEvent::SessionHibernated { .. }))
        .count();
    assert_eq!(hibernated, 1, "only the old task's teardown: {seen:?}");
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, OutEvent::SessionStarted { .. } | OutEvent::Error { .. })),
        "{seen:?}"
    );
    assert!(!holly.is_live(&kid));
}

/// a prompt to a child hibernating behind a slow revert asks for one
/// load (the `Resume` policy), and a root `SetMode` cascading over the
/// non-live child asks for none: one load, one refusal, no start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cascade_over_a_retiring_child_sends_it_nothing() {
    let loader = Arc::new(MapLoader::default());
    let holly = Holly::spawn(EngineConfig {
        unknown_session: UnknownSession::Resume(loader.clone()),
        file_reverter: Some(slow_reverter()),
        ..config(1, Duration::ZERO)
    });
    let mut sub = holly.subscribe();
    let (root, kid) = (SessionId::new("root"), SessionId::new("kid"));
    // Under the `Resume` policy a prompt to a fresh id is refused: spawn it.
    holly
        .send(InMsg::Spawn {
            session: root.clone(),
            parent: None,
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
    let mut log = record_turn(&holly, &mut sub, &root).await;
    spawn_child(&holly, &mut sub, &root, &kid, &mut log).await;
    start_slow_revert(&holly, &kid).await;
    holly.hibernate(kid.clone()).await.unwrap();
    holly.send(InMsg::prompt(kid.clone(), "hi")).await.unwrap();
    holly.send(set_mode(&root, "plan")).await.unwrap();

    let seen = collect(&mut sub, &kid, Duration::from_millis(900)).await;
    let errors = seen
        .iter()
        .filter(|e| matches!(e, OutEvent::Error { .. }))
        .count();
    assert_eq!(errors, 1, "{seen:?}");
    assert!(!seen
        .iter()
        .any(|e| matches!(e, OutEvent::SessionStarted { .. })));
    assert_eq!(
        loader.calls.load(Ordering::SeqCst),
        1,
        "one load, the prompt's"
    );
}

/// A compaction of the parent while its child's new task still waits for the
/// child's old one: the waiting task is a live child, so the successor adopts
/// it (ADR-0234) and the source's close spares it, as on master.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compaction_adopts_a_child_waiting_for_its_old_task() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let (src, kid, succ) = (
        SessionId::new("src"),
        SessionId::new("kid"),
        SessionId::new("succ"),
    );
    child_resume_behind_teardown(&holly, &mut sub, &src, &kid).await;
    holly
        .send(InMsg::Spawn {
            session: succ.clone(),
            parent: None,
            predecessor: Some(src.clone()),
            agent: "general".into(),
            prompt: "continue".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: src.clone(),
        })
        .await
        .unwrap();

    let seen = collect(&mut sub, &kid, Duration::from_millis(900)).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, OutEvent::ParentChanged { parent, .. } if parent == &succ)),
        "{seen:?}"
    );
    assert!(seen
        .iter()
        .any(|e| matches!(e, OutEvent::SessionStarted { .. })));
    assert!(!seen
        .iter()
        .any(|e| matches!(e, OutEvent::SessionEnded { .. })));
    assert!(holly.is_live(&kid));
    let info = holly.session_info(&kid).expect("kid is live");
    assert_eq!(info.parent, Some(succ));
}

/// An explicit `Spawn` of an id whose old task is still tearing down starts
/// a task that waits for the old one, like a resume does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spawn_behind_a_teardown_waits_for_the_old_task() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("respawn");
    record_turn(&holly, &mut sub, &sid).await;
    start_slow_revert(&holly, &sid).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly
        .send(InMsg::Spawn {
            session: sid.clone(),
            parent: None,
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
    assert_b_starts_after_a_exits(&mut sub, &sid).await;
    assert!(holly.is_live(&sid));
}

/// A task that panics while hibernating never retires its seq cell, so it
/// stays live in the registry. The resumed task waits for the panicked one
/// (its exit signal fires on a `JoinError` too) and then shares that cell, so
/// its seqs continue above the ones the runtime minted while it was dying.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_after_a_panicked_teardown_continues_the_sequence() {
    let reverter: FileReverter = Arc::new(|_session, _to_turn| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            panic!("reverter blew up (intended by the test)");
        })
    });
    let holly = Holly::spawn(EngineConfig {
        file_reverter: Some(reverter),
        ..config(1, Duration::ZERO)
    });
    let mut sub = holly.subscribe();
    let sid = SessionId::new("panicky");
    let log = record_turn(&holly, &mut sub, &sid).await;
    start_slow_revert(&holly, &sid).await;
    holly.hibernate(sid.clone()).await.unwrap();

    for _ in 0..20 {
        holly.emit_for_session(&sid, |seq| OutEvent::Error {
            session: sid.clone(),
            seq,
            message: "runtime".into(),
        });
    }
    holly.resume(sid.clone(), log).await.unwrap();
    let mut minted = 0;
    loop {
        match next_for(&mut sub, &sid).await {
            OutEvent::Error { seq, message, .. } if message == "runtime" => {
                minted = minted.max(seq)
            }
            OutEvent::SessionStarted { .. } => break,
            _ => {}
        }
    }
    assert!(minted > 0);
    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    let first = loop {
        if let Some(seq) = next_for(&mut sub, &sid).await.seq() {
            break seq;
        }
    };
    assert!(
        first > minted,
        "B's seq {first} must exceed the runtime's {minted}"
    );
}
