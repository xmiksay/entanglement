//! A resumed sub-agent's mode (#905, ADR-0246): its replayed mode unless its
//! nearest ancestor's current mode is stricter — the ancestor's live mode, or
//! its logged one when it is not running — through every resume path.

use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId, UnknownSession};
use tokio::sync::broadcast::Receiver;

use super::seq_robustness::config;
use super::unknown_session::{MapLoader, Records};

pub(super) const WAIT: Duration = Duration::from_secs(10);

pub(super) fn engine_with(unknown_session: UnknownSession) -> Holly {
    Holly::spawn(EngineConfig {
        mode_ladder: ["research", "plan", "build", "auto"]
            .map(String::from)
            .to_vec(),
        unknown_session,
        ..config(1, Duration::ZERO)
    })
}

fn engine() -> Holly {
    engine_with(UnknownSession::SpawnBlank)
}

/// A root log in the making: every event of `ids`, in broadcast order.
pub(super) struct Recorder {
    pub(super) sub: Receiver<OutEvent>,
    pub(super) ids: Vec<SessionId>,
    pub(super) log: Records,
}

impl Recorder {
    pub(super) fn new(holly: &Holly, ids: &[&SessionId]) -> Self {
        Self {
            sub: holly.subscribe(),
            ids: ids.iter().map(|id| (*id).clone()).collect(),
            log: Vec::new(),
        }
    }

    /// Record until an event matching `pred` arrives, and return it.
    pub(super) async fn until(&mut self, pred: impl Fn(&OutEvent) -> bool) -> OutEvent {
        loop {
            let ev = tokio::time::timeout(WAIT, self.sub.recv())
                .await
                .expect("timed out")
                .expect("event stream open");
            if ev.session().is_some_and(|s| self.ids.contains(s)) {
                self.log.push((None, ev.clone()));
            }
            if pred(&ev) {
                return ev;
            }
        }
    }
}

pub(super) fn is<'a>(
    sid: &'a SessionId,
    kind: fn(&OutEvent) -> bool,
) -> impl Fn(&OutEvent) -> bool + 'a {
    move |ev| ev.session() == Some(sid) && kind(ev)
}

pub(super) fn done(ev: &OutEvent) -> bool {
    matches!(ev, OutEvent::Done { .. })
}

pub(super) fn hibernated(ev: &OutEvent) -> bool {
    matches!(ev, OutEvent::SessionHibernated { .. })
}

/// Root `root` runs a turn, then spawns `child`, which runs one too.
pub(super) async fn tree(holly: &Holly, rec: &mut Recorder, root: &SessionId, child: &SessionId) {
    holly
        .send(InMsg::prompt(root.clone(), "one"))
        .await
        .unwrap();
    rec.until(is(root, done)).await;
    spawn(holly, rec, root, child).await;
}

/// Spawn `child` under `parent` and let it run its first turn.
pub(super) async fn spawn(
    holly: &Holly,
    rec: &mut Recorder,
    parent: &SessionId,
    child: &SessionId,
) {
    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "general".into(),
            prompt: "task".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    rec.until(is(child, done)).await;
}

pub(super) async fn set_mode(holly: &Holly, rec: &mut Recorder, sid: &SessionId, mode: &str) {
    let msg = InMsg::SetMode {
        session: sid.clone(),
        mode: mode.into(),
    };
    holly.send(msg).await.unwrap();
    let want = mode.to_string();
    rec.until(move |ev| matches!(ev, OutEvent::ModeChanged { session, mode } if session == sid && *mode == want))
        .await;
}

pub(super) async fn hibernate(holly: &Holly, rec: &mut Recorder, sid: &SessionId) {
    holly.hibernate(sid.clone()).await.unwrap();
    rec.until(is(sid, hibernated)).await;
}

/// The mode `sid` announces right after it (re)starts.
pub(super) async fn started_mode(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> String {
    let mut started = false;
    loop {
        let ev = tokio::time::timeout(WAIT, sub.recv())
            .await
            .expect("timed out")
            .expect("event stream open");
        match ev {
            OutEvent::SessionStarted { session, .. } if &session == sid => started = true,
            OutEvent::ModeChanged { session, mode } if &session == sid && started => return mode,
            _ => {}
        }
    }
}

/// The whole tree hibernated, after the root narrowed to `research` while
/// the child was already hibernated in `build`.
async fn hibernated_tree_log() -> (Records, SessionId) {
    let holly = engine();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "research").await;
    hibernate(&holly, &mut rec, &root).await;
    (rec.log, child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hibernated_child_resumes_in_its_live_ancestors_narrower_mode() {
    let holly = engine();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "research").await;

    let mut sub = holly.subscribe();
    holly.resume(child.clone(), rec.log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "research");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_narrowed_below_its_parent_keeps_its_mode() {
    let holly = engine();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    set_mode(&holly, &mut rec, &child, "research").await;
    hibernate(&holly, &mut rec, &child).await;

    let mut sub = holly.subscribe();
    holly.resume(child.clone(), rec.log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "research");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_of_a_hibernated_tree_takes_its_ancestors_logged_mode() {
    let holly = engine();
    let (root, child) = (SessionId::new("root"), SessionId::new("kid"));
    let mut rec = Recorder::new(&holly, &[&root, &child]);
    tree(&holly, &mut rec, &root, &child).await;
    hibernate(&holly, &mut rec, &child).await;
    set_mode(&holly, &mut rec, &root, "research").await;
    hibernate(&holly, &mut rec, &root).await;

    let mut sub = holly.subscribe();
    holly.resume(child.clone(), rec.log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "research");
    assert!(!holly.is_live(&root), "the ancestor stays hibernated");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_process_resume_follows_the_same_rule() {
    let (log, child) = hibernated_tree_log().await;
    let holly = engine();
    let mut sub = holly.subscribe();
    holly.resume(child.clone(), log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "research");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lazy_load_follows_the_same_rule() {
    let (log, child) = hibernated_tree_log().await;
    let loader = MapLoader::default();
    loader.logs.lock().unwrap().insert(child.clone(), log);
    let holly = engine_with(UnknownSession::Resume(Arc::new(loader)));
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(child.clone(), "hi"))
        .await
        .unwrap();
    assert_eq!(started_mode(&mut sub, &child).await, "research");
}

/// A grandchild under a hibernated child: the child's own mode is narrowed
/// by the live root first, so the grandchild takes the root's mode, not the
/// child's logged one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hibernated_ancestor_is_itself_narrowed_by_its_ancestors() {
    let holly = engine();
    let (root, child, grand) = (
        SessionId::new("root"),
        SessionId::new("kid"),
        SessionId::new("grand"),
    );
    let mut rec = Recorder::new(&holly, &[&root, &child, &grand]);
    tree(&holly, &mut rec, &root, &child).await;
    spawn(&holly, &mut rec, &child, &grand).await;
    holly.hibernate(child.clone()).await.unwrap();
    // The two tear down concurrently: wait for both, in either order.
    for _ in 0..2 {
        rec.until(|ev| {
            hibernated(ev) && (ev.session() == Some(&child) || ev.session() == Some(&grand))
        })
        .await;
    }
    set_mode(&holly, &mut rec, &root, "research").await;

    let mut sub = holly.subscribe();
    holly.resume(grand.clone(), rec.log).await.unwrap();
    assert_eq!(started_mode(&mut sub, &grand).await, "research");
}
