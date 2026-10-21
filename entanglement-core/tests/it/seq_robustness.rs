//! Seq robustness (ADR-0244): one task per id across a hibernate → resume
//! (#892), and delivery in seq order across concurrent emitters (#893).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, FileRevertReport, FileReverter, Holly, InMsg, Llm,
    LlmEvent, LlmRequest, LlmResponse, LlmStream, OutEvent, SessionId, StopReason, Usage,
};
use tokio::sync::broadcast::Receiver;

const WAIT: Duration = Duration::from_secs(10);

pub(super) async fn next_for(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> OutEvent {
    loop {
        let ev = tokio::time::timeout(WAIT, sub.recv())
            .await
            .expect("timed out")
            .expect("event stream open");
        if ev.session() == Some(sid) {
            return ev;
        }
    }
}

/// Each round streams `chunks` text deltas, yielding between them, so the
/// session task keeps emitting while another emitter runs.
struct Chunky {
    chunks: usize,
    delay: Duration,
}

#[async_trait]
impl Llm for Chunky {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(self.delay).await;
        let mut events: Vec<anyhow::Result<LlmEvent>> = (0..self.chunks)
            .map(|i| Ok(LlmEvent::Text(format!("{i} "))))
            .collect();
        events.push(Ok(LlmEvent::Finish {
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        }));
        let delay = self.delay;
        Ok(Box::pin(futures::StreamExt::then(
            futures::stream::iter(events),
            move |ev| async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay / 10).await;
                }
                tokio::task::yield_now().await;
                ev
            },
        )))
    }
}

pub(super) fn config(chunks: usize, delay: Duration) -> EngineConfig {
    EngineConfig {
        llm_factory: Arc::new(move || Box::new(Chunky { chunks, delay }) as Box<dyn Llm>),
        ..EngineConfig::default()
    }
}

fn engine(chunks: usize, delay: Duration) -> Holly {
    Holly::spawn(config(chunks, delay))
}

/// Run one turn on `sid` and return it as a resume log.
pub(super) async fn record_turn(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<(Option<InMsg>, OutEvent)> {
    let prompt = InMsg::prompt(sid.clone(), "one");
    holly.send(prompt.clone()).await.unwrap();
    let (mut log, mut paired) = (Vec::new(), Some(prompt));
    loop {
        let ev = next_for(sub, sid).await;
        let done = matches!(ev, OutEvent::Done { .. });
        log.push((paired.take(), ev));
        if done {
            return log;
        }
    }
}

/// The old task A's last event, `SessionHibernated`, reaches the outbox
/// before the resumed task B's `SessionStarted`, and nothing A authors
/// follows it. B is idle (the log ends on a finished turn), so anything
/// content-like after its start could only come from A.
pub(super) async fn assert_b_starts_after_a_exits(sub: &mut Receiver<OutEvent>, sid: &SessionId) {
    let mut hibernated = false;
    loop {
        match next_for(sub, sid).await {
            OutEvent::SessionHibernated { .. } => hibernated = true,
            OutEvent::SessionStarted { .. } => break,
            _ => {}
        }
    }
    assert!(hibernated, "{sid}: B started before A exited");
    let late = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            let ev = next_for(sub, sid).await;
            if matches!(
                ev,
                OutEvent::TextDelta { .. }
                    | OutEvent::Done { .. }
                    | OutEvent::Error { .. }
                    | OutEvent::FilesReverted { .. }
                    | OutEvent::SessionHibernated { .. }
            ) {
                return ev;
            }
        }
    })
    .await;
    assert!(late.is_err(), "{sid}: A emitted after B started: {late:?}");
}

/// #892: A is inside an await that doesn't watch its inbox (a
/// slow file revert) when the hibernate and the resume land, so its teardown
/// is far behind B's start. B still starts only after A exited, and the
/// supervisor keeps routing other sessions while it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_behind_a_slow_teardown_waits_without_stalling_others() {
    let holly = Holly::spawn(EngineConfig {
        file_reverter: Some(slow_reverter()),
        ..config(1, Duration::ZERO)
    });
    let mut sub = holly.subscribe();
    let sid = SessionId::new("slow");
    let log = record_turn(&holly, &mut sub, &sid).await;

    holly
        .send(InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();

    // While A is still reverting, another session is served end to end:
    // the wait for A's exit happens off the supervisor loop.
    let other = SessionId::new("other");
    holly
        .send(InMsg::prompt(other.clone(), "hi"))
        .await
        .unwrap();
    loop {
        let ev = tokio::time::timeout(WAIT, sub.recv())
            .await
            .expect("timed out")
            .expect("event stream open");
        match ev {
            OutEvent::Done { session, .. } if session == other => break,
            OutEvent::SessionHibernated { session, .. } if session == sid => {
                panic!("the other session waited for A's teardown")
            }
            OutEvent::SessionStarted { session, .. } if session == sid => {
                panic!("B started while A was still reverting")
            }
            _ => {}
        }
    }

    assert_b_starts_after_a_exits(&mut sub, &sid).await;
    assert!(holly.is_live(&sid));
}

pub(super) fn slow_reverter() -> FileReverter {
    Arc::new(|_session, to_turn| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(FileRevertReport {
                to_turn: to_turn.unwrap_or(0),
                restored: Vec::new(),
                skipped: Vec::new(),
            })
        })
    })
}

/// A close is never blocked by a stuck teardown (ADR-0244 §4). A prompt sent
/// after the hibernate starts a new task for the id, which waits for the old
/// one; the close tombstones the id at once, so a later prompt is refused
/// before the old task even finishes, and the waiting task, closed before it
/// ever ran, ends without emitting anything (its prompt is dropped with it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_is_not_blocked_by_a_stuck_teardown() {
    let holly = Holly::spawn(EngineConfig {
        file_reverter: Some(slow_reverter()),
        ..config(1, Duration::ZERO)
    });
    let mut sub = holly.subscribe();
    let sid = SessionId::new("stuck");
    record_turn(&holly, &mut sub, &sid).await;
    holly
        .send(InMsg::RevertFiles {
            session: sid.clone(),
            to_turn: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "waits"))
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::prompt(sid.clone(), "again"))
        .await
        .unwrap();
    loop {
        match next_for(&mut sub, &sid).await {
            OutEvent::Error { message, .. } if message.contains("closed") => break,
            OutEvent::SessionHibernated { .. } => {
                panic!("the close waited for the stuck teardown")
            }
            ev => assert!(!matches!(ev, OutEvent::SessionStarted { .. }), "{ev:?}"),
        }
    }
    // Past the old task's exit: the waiting task never starts or speaks.
    let mut hibernated = false;
    let _ = tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            match next_for(&mut sub, &sid).await {
                OutEvent::SessionHibernated { .. } => hibernated = true,
                OutEvent::FilesReverted { .. } => {}
                ev => panic!("the closed waiting task emitted {ev:?}"),
            }
        }
    })
    .await;
    assert!(hibernated, "the old task finished its teardown");
    assert!(!holly.is_live(&sid));
}

/// Drain `sid`'s seq-bearing events in delivery order until `want` arrived,
/// asserting every one is above the previous.
async fn assert_delivered_in_order(sub: &mut Receiver<OutEvent>, sid: &SessionId, want: usize) {
    let (mut last, mut seen) = (0, 0);
    while seen < want {
        let ev = next_for(sub, sid).await;
        if let Some(seq) = ev.seq() {
            assert!(seq > last, "seq {seq} delivered after {last}");
            last = seq;
            seen += 1;
        }
    }
}

fn spam(holly: &Holly, sid: &SessionId, n: usize) -> std::thread::JoinHandle<()> {
    let (holly, sid) = (holly.clone(), sid.clone());
    std::thread::spawn(move || {
        for _ in 0..n {
            holly.emit_for_session(&sid, |seq| OutEvent::Error {
                session: sid.clone(),
                seq,
                message: "runtime".into(),
            });
        }
    })
}

/// #893: the session task streaming and two runtime emitters racing it for
/// one session — every event arrives in seq order, so a `seq > last_seen`
/// dedupe folds all of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_emitters_for_one_session_deliver_in_seq_order() {
    const CHUNKS: usize = 200;
    const PER_THREAD: usize = 200;
    let holly = engine(CHUNKS, Duration::ZERO);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("race");
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    while !matches!(next_for(&mut sub, &sid).await, OutEvent::TextDelta { .. }) {}

    let threads = [
        spam(&holly, &sid, PER_THREAD),
        spam(&holly, &sid, PER_THREAD),
    ];
    // The stream's remaining deltas, the turn's tail and both threads' events
    // race; well within the outbox's capacity, so nothing lags.
    assert_delivered_in_order(&mut sub, &sid, CHUNKS / 2 + 2 * PER_THREAD).await;
    for t in threads {
        t.join().unwrap();
    }
}

/// #893 for a retired session: two runtime emitters minting from the
/// retired counter still deliver in seq order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_emits_for_a_retired_session_deliver_in_seq_order() {
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| {
            Box::new(Fixed(LlmResponse {
                text: "ok".into(),
                ..Default::default()
            })) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    });
    let mut sub = holly.subscribe();
    let sid = SessionId::new("retired");
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    while !matches!(next_for(&mut sub, &sid).await, OutEvent::Done { .. }) {}
    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    while !matches!(
        next_for(&mut sub, &sid).await,
        OutEvent::SessionEnded { .. }
    ) {}

    let threads = [spam(&holly, &sid, 300), spam(&holly, &sid, 300)];
    assert_delivered_in_order(&mut sub, &sid, 600).await;
    for t in threads {
        t.join().unwrap();
    }
}

struct Fixed(LlmResponse);

#[async_trait]
impl Llm for Fixed {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        Ok(stream_from_response(self.0.clone()))
    }
}

/// #893 for supervisor errors: the closed-id refusals the supervisor emits
/// race a runtime emitter for the same retired id, and still arrive in seq
/// order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervisor_errors_and_runtime_emits_deliver_in_seq_order() {
    const N: usize = 300;
    let holly = engine(1, Duration::ZERO);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("closed");
    record_turn(&holly, &mut sub, &sid).await;
    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    while !matches!(
        next_for(&mut sub, &sid).await,
        OutEvent::SessionEnded { .. }
    ) {}

    let runtime = spam(&holly, &sid, N);
    let prompts = {
        let (holly, sid) = (holly.clone(), sid.clone());
        tokio::spawn(async move {
            for _ in 0..N {
                holly.send(InMsg::prompt(sid.clone(), "x")).await.unwrap();
            }
        })
    };
    assert_delivered_in_order(&mut sub, &sid, 2 * N).await;
    prompts.await.unwrap();
    runtime.join().unwrap();
}
