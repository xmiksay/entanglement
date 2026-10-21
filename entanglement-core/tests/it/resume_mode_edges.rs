//! Edges of the resumed sub-agent mode rule (#905, ADR-0246): a child spawned
//! under a hibernated parent, a compacted ancestor in a fresh process, the
//! mode-scoped overlay on a clamped resume, and the supervisor's cache while
//! the old task still tears down.

use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{
    EngineConfig, Holly, InMsg, Llm, OutEvent, SessionId, ToolOverlayEntry, UnknownSession,
};

use super::resume_mode::{engine_with, hibernate, set_mode, spawn, started_mode, tree, Recorder};
use super::resume_tail::{RecordingLlm, Seen};
use super::seq_one_task::start_slow_revert;
use super::seq_robustness::{config, record_turn, slow_reverter};
use super::unknown_session::Records;

fn engine() -> Holly {
    engine_with(UnknownSession::SpawnBlank)
}

/// A laddered engine whose file reverts take 400ms.
fn reverting_engine() -> Holly {
    Holly::spawn(EngineConfig {
        mode_ladder: ["research", "plan", "build", "auto"]
            .map(String::from)
            .to_vec(),
        file_reverter: Some(slow_reverter()),
        ..config(1, Duration::ZERO)
    })
}

/// A child spawned under a hibernated parent takes the mode an ancestor
/// narrowed that parent to, not the parent's stale wider one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spawn_under_a_hibernated_parent_takes_the_narrowed_mode() {
    let holly = engine();
    let (root, child, grand) = (
        SessionId::new("root"),
        SessionId::new("kid"),
        SessionId::new("grand"),
    );
    let mut rec = Recorder::new(&holly, &[&root, &child, &grand]);
    tree(&holly, &mut rec, &root, &child).await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "research").await;

    let mut sub = holly.subscribe();
    spawn(&holly, &mut rec, &child, &grand).await;
    assert_eq!(started_mode(&mut sub, &grand).await, "research");
}

fn ev(json: &str) -> (Option<InMsg>, OutEvent) {
    (None, serde_json::from_str(json).expect("event"))
}

/// In a fresh process nothing is closed, yet a compacted-away parent still
/// stands for its successor: the child takes the successor's narrower mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compacted_parent_stands_for_its_successor_in_a_fresh_process() {
    let log: Records = vec![
        ev(
            r#"{"kind":"session_started","session":"p","parent":null,"agent":"general","model":null,"root":true,"ts":1}"#,
        ),
        ev(r#"{"kind":"mode_changed","session":"p","mode":"build"}"#),
        ev(
            r#"{"kind":"session_started","session":"kid","parent":"p","agent":"general","model":null,"root":false,"ts":2}"#,
        ),
        ev(r#"{"kind":"mode_changed","session":"kid","mode":"build"}"#),
        ev(r#"{"kind":"session_hibernated","session":"kid","ts":3}"#),
        ev(
            r#"{"kind":"session_started","session":"s","parent":null,"predecessor":"p","agent":"general","model":null,"root":true,"ts":4}"#,
        ),
        ev(r#"{"kind":"mode_changed","session":"s","mode":"research"}"#),
        ev(r#"{"kind":"session_ended","session":"p","ts":5}"#),
    ];
    let holly = engine();
    let mut sub = holly.subscribe();
    let kid = SessionId::new("kid");
    holly.resume(kid.clone(), log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &kid).await, "research");
}

/// A clamped resume is a real switch: the child's mode-scoped overlay goes,
/// so nothing re-announces it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clamped_resume_drops_the_mode_scoped_overlay() {
    let holly = engine();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    let overlay = InMsg::SetToolOverlay {
        session: child.clone(),
        entries: vec![ToolOverlayEntry::allow("bash")],
    };
    holly.send(overlay).await.unwrap();
    rec.until(|e| matches!(e, OutEvent::ToolOverlayChanged { session, .. } if *session == child))
        .await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "research").await;

    let mut sub = holly.subscribe();
    holly.resume(child.clone(), rec.log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "research");
    let revived = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if let Ok(OutEvent::ToolOverlayChanged { session, entries }) = sub.recv().await {
                if session == child && !entries.is_empty() {
                    return entries;
                }
            }
        }
    })
    .await;
    assert!(revived.is_err(), "overlay re-announced: {revived:?}");
}

/// The old task applies a `SetMode research` during its teardown, after the
/// head read its records: the supervisor's cache is the newer word.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_retiring_cache_beats_a_lagging_log() {
    let holly = reverting_engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("lag");
    let log = record_turn(&holly, &mut sub, &sid).await;
    start_slow_revert(&holly, &sid).await;
    let narrow = InMsg::SetMode {
        session: sid.clone(),
        mode: "research".into(),
    };
    holly.send(narrow).await.unwrap();
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &sid).await, "research");
}

/// The cache never widens: records narrower than the retiring task's mode (a
/// head's forced read-only replay) keep the narrower mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_retiring_cache_never_widens_the_records() {
    let holly = reverting_engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("forced");
    let mut log = record_turn(&holly, &mut sub, &sid).await;
    log.push(ev(
        r#"{"kind":"mode_changed","session":"forced","mode":"research"}"#,
    ));
    start_slow_revert(&holly, &sid).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &sid).await, "research");
}

/// A widening cascade never widens a hibernated child's cache entry, which
/// seeds a grandchild spawned under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_widening_cascade_keeps_a_hibernated_childs_narrow_mode() {
    let holly = engine();
    let (root, child, grand) = (
        SessionId::new("root"),
        SessionId::new("kid"),
        SessionId::new("grand"),
    );
    let mut rec = Recorder::new(&holly, &[&root, &child, &grand]);
    tree(&holly, &mut rec, &root, &child).await;
    set_mode(&holly, &mut rec, &child, "research").await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "auto").await;

    let mut sub = holly.subscribe();
    spawn(&holly, &mut rec, &child, &grand).await;
    assert_eq!(started_mode(&mut sub, &grand).await, "research");
}

/// An ancestor narrowed and widened back while the child was hibernated:
/// the child resumes in the ancestor's current mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ancestor_narrowed_then_widened_back_leaves_the_child_as_it_was() {
    let holly = engine();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "research").await;
    set_mode(&holly, &mut rec, &root, "build").await;
    // Let the child's watcher release its retiring entry.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut sub = holly.subscribe();
    holly.resume(child.clone(), rec.log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "build");
}

/// Records taken before the live root's `SetMode research`: the live mode
/// wins over what the records say about the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_ancestor_beats_stale_records() {
    let holly = engine();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    hibernate(&holly, &mut rec, &child).await;
    let stale = rec.log.clone();
    set_mode(&holly, &mut rec, &root, "research").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut sub = holly.subscribe();
    holly.resume(child.clone(), stale).await.unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "research");
}

/// A clamped resume tells the model what changed, once, like a live switch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clamped_resume_announces_the_transition_to_the_model() {
    let seen: Seen = Arc::default();
    let seen2 = seen.clone();
    let holly = Holly::spawn(EngineConfig {
        mode_ladder: ["research", "plan", "build", "auto"]
            .map(String::from)
            .to_vec(),
        llm_factory: Arc::new(move || Box::new(RecordingLlm(seen2.clone())) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "research").await;

    holly.resume(child.clone(), rec.log.clone()).await.unwrap();
    holly
        .send(InMsg::prompt(child.clone(), "go"))
        .await
        .unwrap();
    rec.until(|e| matches!(e, OutEvent::Done { session, .. } if *session == child))
        .await;
    let requests = seen.lock().unwrap();
    let last = format!("{:?}", requests.last().expect("the resumed turn ran"));
    assert!(last.contains("changed from build"), "{last}");
}
