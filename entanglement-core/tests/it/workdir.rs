//! Session working directories (#764, ADR-0224): `SetWorkdir` and
//! `Spawn.workdir` are validated through the runtime's resolver seam, applied
//! between turns, stamped on every `ToolExec`, inherited by new children,
//! successors and forks, replayed — and told to the model through the
//! per-round trailing notice, never the system prompt or `Context`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::session::Session;
use entanglement_core::{
    ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmEvent, LlmRequest, LlmStream, OutEvent,
    SessionId, StopReason, ToolCall, Usage, WorkdirResolver,
};
use futures::{stream, StreamExt};
use tokio::sync::broadcast::Receiver;

use super::successor_model::{compact, next, start_and_switch, Harness, Path};

/// Every request's `(system, trailing_notice)`.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

type Calls = Arc<Mutex<Vec<(Option<String>, String, Option<ApprovalScope>)>>>;

/// Accepts any directory not starting with `bad`, joining a relative one onto
/// the current workdir (the root is `/p`), and records every call.
fn resolver(calls: Calls) -> WorkdirResolver {
    Arc::new(move |current, dir, approve| {
        calls
            .lock()
            .unwrap()
            .push((current.map(String::from), dir.to_string(), approve));
        if dir.starts_with("bad") {
            return Err("not a directory".into());
        }
        Ok(if dir.starts_with('/') {
            dir.to_string()
        } else {
            format!("{}/{dir}", current.unwrap_or("/p"))
        })
    })
}

/// Every request's `(system, trailing_notice)`; the first call parks the
/// turn on a tool call when `tool_first`.
struct Recording {
    seen: Seen,
    calls: usize,
    tool_first: bool,
}

#[async_trait]
impl Llm for Recording {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen
            .lock()
            .unwrap()
            .push((req.system.to_string(), req.trailing_notice.clone()));
        self.calls += 1;
        let first = if self.tool_first && self.calls == 1 {
            LlmEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: "{}".into(),
                provider_meta: None,
            })
        } else {
            LlmEvent::Text("ok".into())
        };
        let stop = if self.tool_first && self.calls == 1 {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        Ok(stream::iter(vec![
            Ok(first),
            Ok(LlmEvent::Finish {
                stop_reason: Some(stop),
                usage: Usage::default(),
            }),
        ])
        .boxed())
    }
}

struct Fixture {
    holly: Holly,
    cfg: EngineConfig,
    calls: Calls,
    seen: Seen,
}

fn fixture(tool_first: bool, with_resolver: bool) -> Fixture {
    let calls: Calls = Arc::default();
    let seen: Seen = Arc::default();
    let for_factory = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(Recording {
                seen: for_factory.clone(),
                calls: 0,
                tool_first,
            }) as Box<dyn Llm>
        }),
        workdir_resolver: with_resolver.then(|| resolver(calls.clone())),
        reoffer_interval: None,
        ..EngineConfig::default()
    };
    Fixture {
        holly: Holly::spawn(cfg.clone()),
        cfg,
        calls,
        seen,
    }
}

async fn matching(sub: &mut Receiver<OutEvent>, pred: impl Fn(&OutEvent) -> bool) -> OutEvent {
    loop {
        let ev = next(sub).await;
        if pred(&ev) {
            return ev;
        }
    }
}

/// Wait for `sid`'s next `WorkdirChanged` and return its `dir`.
async fn workdir_of(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> String {
    match matching(
        sub,
        |e| matches!(e, OutEvent::WorkdirChanged { session, .. } if session == sid),
    )
    .await
    {
        OutEvent::WorkdirChanged { dir, .. } => dir,
        _ => unreachable!(),
    }
}

fn set(sid: &SessionId, dir: &str) -> InMsg {
    InMsg::SetWorkdir {
        session: sid.clone(),
        dir: dir.into(),
        approve: None,
    }
}

fn spawn(sid: &str, parent: Option<&SessionId>, workdir: Option<&str>) -> InMsg {
    InMsg::Spawn {
        session: SessionId::new(sid),
        parent: parent.cloned(),
        predecessor: None,
        agent: "general".into(),
        prompt: String::new(),
        user: None,
        model: None,
        mode: None,
        workdir: workdir.map(String::from),
    }
}

async fn turn(f: &Fixture, sub: &mut Receiver<OutEvent>, sid: &SessionId, text: &str) {
    f.holly
        .send(InMsg::prompt(sid.clone(), text))
        .await
        .unwrap();
    matching(
        sub,
        |e| matches!(e, OutEvent::Done { session, .. } if session == sid),
    )
    .await;
}

#[tokio::test]
async fn set_workdir_is_announced_and_named_in_the_notice_but_not_the_system_prompt() {
    let f = fixture(false, true);
    let sid = SessionId::new("s1");
    let mut sub = f.holly.subscribe();
    turn(&f, &mut sub, &sid, "before").await;

    f.holly.send(set(&sid, "sub")).await.unwrap();
    assert_eq!(workdir_of(&mut sub, &sid).await, "/p/sub");
    turn(&f, &mut sub, &sid, "after").await;

    let seen = f.seen.lock().unwrap().clone();
    let (before, after) = (&seen[0], &seen[1]);
    assert_eq!(before.0, after.0, "the system prompt never changes");
    assert!(
        !before.1.as_deref().unwrap_or("").contains("workdir"),
        "{before:?}"
    );
    assert!(
        after
            .1
            .as_deref()
            .unwrap_or("")
            .contains("[workdir: /p/sub"),
        "{after:?}"
    );
}

#[tokio::test]
async fn a_relative_dir_resolves_against_the_current_workdir() {
    let f = fixture(false, true);
    let sid = SessionId::new("s1");
    let mut sub = f.holly.subscribe();
    f.holly.send(set(&sid, "sub")).await.unwrap();
    workdir_of(&mut sub, &sid).await;
    f.holly
        .send(InMsg::SetWorkdir {
            session: sid.clone(),
            dir: "deep".into(),
            approve: Some(ApprovalScope::Session),
        })
        .await
        .unwrap();
    assert_eq!(workdir_of(&mut sub, &sid).await, "/p/sub/deep");
    let calls = f.calls.lock().unwrap().clone();
    assert_eq!(
        calls[1],
        (
            Some("/p/sub".into()),
            "deep".into(),
            Some(ApprovalScope::Session)
        ),
        "the approval reaches the runtime's resolver"
    );
}

#[tokio::test]
async fn a_refused_dir_errors_and_changes_nothing() {
    for with_resolver in [true, false] {
        let f = fixture(false, with_resolver);
        let sid = SessionId::new("s1");
        let mut sub = f.holly.subscribe();
        f.holly.send(set(&sid, "bad")).await.unwrap();
        let err = matching(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
        assert!(
            matches!(&err, OutEvent::Error { message, .. } if message.contains("cannot change workdir")),
            "{err:?}"
        );
        turn(&f, &mut sub, &sid, "hi").await;
        let seen = f.seen.lock().unwrap().clone();
        assert!(!seen[0].1.as_deref().unwrap_or("").contains("workdir"));
    }
}

/// Every call of a turn runs under one workdir: a `/cd` while the turn is
/// parked applies only once it ends, and the parked call keeps the old one.
#[tokio::test]
async fn tool_exec_carries_the_workdir_and_a_mid_turn_change_waits() {
    let f = fixture(true, true);
    let sid = SessionId::new("s1");
    let mut sub = f.holly.subscribe();
    f.holly.send(set(&sid, "sub")).await.unwrap();
    workdir_of(&mut sub, &sid).await;

    f.holly
        .send(InMsg::prompt(sid.clone(), "go"))
        .await
        .unwrap();
    let exec = matching(&mut sub, |e| matches!(e, OutEvent::ToolExec { .. })).await;
    let OutEvent::ToolExec {
        request_id,
        workdir,
        ..
    } = exec
    else {
        unreachable!()
    };
    assert_eq!(workdir.as_deref(), Some("/p/sub"));

    f.holly.send(set(&sid, "/p/other")).await.unwrap();
    let early = tokio::time::timeout(Duration::from_millis(150), async {
        matching(&mut sub, |e| matches!(e, OutEvent::WorkdirChanged { .. })).await
    })
    .await;
    assert!(early.is_err(), "not applied while the turn is parked");

    f.holly
        .send(InMsg::tool_result(sid.clone(), request_id, "done"))
        .await
        .unwrap();
    matching(&mut sub, |e| matches!(e, OutEvent::Done { .. })).await;
    assert_eq!(workdir_of(&mut sub, &sid).await, "/p/other");
}

#[tokio::test]
async fn a_spawn_starts_in_its_workdir_and_children_inherit() {
    let f = fixture(false, true);
    let mut sub = f.holly.subscribe();
    let root = SessionId::new("root");
    f.holly
        .send(spawn("root", None, Some("sub")))
        .await
        .unwrap();
    assert_eq!(workdir_of(&mut sub, &root).await, "/p/sub");

    let kid = SessionId::new("kid");
    f.holly.send(spawn("kid", Some(&root), None)).await.unwrap();
    assert_eq!(workdir_of(&mut sub, &kid).await, "/p/sub", "inherited");

    // A later `/cd` moves the parent only; a child spawned after it follows.
    f.holly.send(set(&root, "/p/b")).await.unwrap();
    assert_eq!(workdir_of(&mut sub, &root).await, "/p/b");
    let kid2 = SessionId::new("kid2");
    f.holly
        .send(spawn("kid2", Some(&root), Some("c")))
        .await
        .unwrap();
    assert_eq!(
        workdir_of(&mut sub, &kid2).await,
        "/p/b/c",
        "an explicit child workdir resolves against the parent's"
    );
    let early = tokio::time::timeout(Duration::from_millis(150), async {
        matching(
            &mut sub,
            |e| matches!(e, OutEvent::WorkdirChanged { session, .. } if session == &kid),
        )
        .await
    })
    .await;
    assert!(early.is_err(), "an existing child keeps its workdir");
}

#[tokio::test]
async fn a_spawn_with_a_bad_workdir_is_refused() {
    let f = fixture(false, true);
    let mut sub = f.holly.subscribe();
    f.holly.send(spawn("x", None, Some("bad"))).await.unwrap();
    let ev = next(&mut sub).await;
    assert!(
        matches!(&ev, OutEvent::Error { message, .. } if message.contains("cannot spawn in workdir")),
        "{ev:?}"
    );
    assert!(!f.holly.is_live(&SessionId::new("x")));
}

#[tokio::test]
async fn replay_and_resume_restore_the_workdir() {
    let f = fixture(false, true);
    let sid = SessionId::new("s1");
    let mut sub = f.holly.subscribe();
    let mut log_sub = f.holly.subscribe();
    turn(&f, &mut sub, &sid, "one").await;
    f.holly.send(set(&sid, "sub")).await.unwrap();
    workdir_of(&mut sub, &sid).await;
    let mut log = Vec::new();
    while let Ok(ev) = log_sub.try_recv() {
        log.push((None, ev));
    }
    let replayed = Session::replay(&log, &f.cfg, &sid).expect("replay");
    assert_eq!(replayed.workdir.as_deref(), Some("/p/sub"));

    let other = Holly::spawn(f.cfg.clone());
    let mut sub = other.subscribe();
    other.resume(sid.clone(), log).await.unwrap();
    assert_eq!(
        workdir_of(&mut sub, &sid).await,
        "/p/sub",
        "re-announced on resume"
    );
}

/// A replayed workdir the resolver no longer accepts (gone, swapped out,
/// grant expired) falls back to the root on resume, with an `Error` saying so.
#[tokio::test]
async fn a_stale_workdir_falls_back_to_the_root_on_resume() {
    let f = fixture(false, true);
    let sid = SessionId::new("s1");
    let log = vec![
        (
            None,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                predecessor: None,
                agent: "general".into(),
                model: None,
                root: true,
                ts: 0,
                user: None,
            },
        ),
        (
            None,
            OutEvent::WorkdirChanged {
                session: sid.clone(),
                dir: "bad-gone".into(),
            },
        ),
    ];
    let mut sub = f.holly.subscribe();
    f.holly.resume(sid.clone(), log).await.unwrap();
    let err = matching(&mut sub, |e| matches!(e, OutEvent::Error { .. })).await;
    assert!(
        matches!(&err, OutEvent::Error { message, .. } if message.contains("no longer usable")),
        "{err:?}"
    );
    assert_eq!(workdir_of(&mut sub, &sid).await, "/p/.", "the root, logged");
    turn(&f, &mut sub, &sid, "hi").await;
    let seen = f.seen.lock().unwrap().clone();
    assert!(!seen[0].1.as_deref().unwrap_or("").contains("bad-gone"));
}

#[tokio::test]
async fn set_workdir_is_refused_from_the_wire() {
    let holly = Holly::spawn(EngineConfig::default());
    let msg = set(&SessionId::new("s"), "sub");
    assert!(!msg.wire_allowed());
    assert!(holly.send_from_wire(msg).await.is_err());
}

#[tokio::test]
async fn every_compaction_successor_and_a_fork_keep_the_workdir() {
    for path in Path::ALL {
        let h = Harness::new();
        let calls: Calls = Arc::default();
        let cfg = EngineConfig {
            workdir_resolver: Some(resolver(calls)),
            ..h.config(path.auto_compact())
        };
        let holly = Holly::spawn(cfg.clone());
        let sid = SessionId::new("s1");
        let mut sub = holly.subscribe();
        start_and_switch(&holly, &mut sub, &sid).await;
        holly.send(set(&sid, "sub")).await.unwrap();
        workdir_of(&mut sub, &sid).await;

        let (successor, events) = compact(&holly, &mut sub, &sid, path).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, OutEvent::WorkdirChanged { dir, .. } if dir == "/p/sub")),
            "{path:?}: {events:?}"
        );
        let records: Vec<_> = events.into_iter().map(|e| (None, e)).collect();
        let replayed = Session::replay(&records, &cfg, &successor).expect("replay");
        assert_eq!(replayed.workdir.as_deref(), Some("/p/sub"), "{path:?}");

        holly
            .send(InMsg::ForkSession {
                session: successor.clone(),
                at_turn: None,
            })
            .await
            .unwrap();
        let mut seen = Vec::new();
        let fork = loop {
            let ev = next(&mut sub).await;
            if let OutEvent::SessionForked { session, .. } = &ev {
                break session.clone();
            }
            seen.push(ev);
        };
        assert!(
            seen.iter().any(|e| matches!(
                e,
                OutEvent::WorkdirChanged { session, dir } if session == &fork && dir == "/p/sub"
            )),
            "{path:?}: the fork starts in its source's workdir"
        );
    }
}
