//! Resume cascades over the spawn sub-tree (#415): a root's persisted log
//! carries its whole spawn sub-tree (children fold into the root file,
//! ADR-0020), so resuming the root must also re-materialize any sub-agent
//! still "live" as of where recording stopped — otherwise the parent's
//! reconstructed `children` mirror names ids with no task behind them, and
//! touching one lazily respawns it *blank*, silently discarding its history.
//! These drive `Holly` end-to-end, mirroring `hibernate.rs`'s style.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    Message, OutEvent, SessionId,
};

/// Messages the provider saw on each round-trip, shared across every session an
/// engine builds so a test can assert reconstructed context.
type Seen = Arc<Mutex<Vec<Vec<Message>>>>;
type Responses = Arc<Mutex<VecDeque<LlmResponse>>>;

struct RecordingLlm {
    responses: Responses,
    seen: Seen,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| LlmResponse {
                text: "assistant-reply".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

fn engine() -> (Holly, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let responses: Responses = Arc::new(Mutex::new(VecDeque::new()));
    let seen2 = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                responses: responses.clone(),
                seen: seen2.clone(),
            }) as Box<dyn Llm>
        }),
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), seen)
}

async fn recv_until(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    pred: impl Fn(&OutEvent) -> bool,
) -> OutEvent {
    loop {
        let recv = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for a matching event");
        match recv {
            Ok(ev) if pred(&ev) => return ev,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => panic!("event stream closed before a matching event"),
        }
    }
}

/// Collect a faithful resume log for `sid`: every event up to and including its
/// first `Done`, with `prompt` tagged onto the first event.
async fn record_turn(
    sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    prompt: Option<InMsg>,
) -> Vec<(Option<InMsg>, OutEvent)> {
    let mut records = Vec::new();
    let mut pending = prompt;
    loop {
        let ev = recv_until(sub, |e| e.session() == Some(sid)).await;
        let done = matches!(&ev, OutEvent::Done { .. });
        records.push((pending.take(), ev));
        if done {
            break;
        }
    }
    records
}

#[tokio::test]
async fn resume_cascades_over_a_live_spawned_child() {
    let (holly, seen) = engine();
    let mut sub = holly.subscribe();

    let parent = SessionId::new("resume-parent");
    let child = SessionId::new("resume-child");

    // Parent runs one turn, then spawns a child that runs its own turn.
    holly
        .send(InMsg::prompt(parent.clone(), "start"))
        .await
        .unwrap();
    let mut log = record_turn(
        &mut sub,
        &parent,
        Some(InMsg::prompt(parent.clone(), "start")),
    )
    .await;

    holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: Some(parent.clone()),
            predecessor: None,
            agent: "build".into(),
            prompt: "child task".into(),
        })
        .await
        .unwrap();
    // `Spawn` itself is never persisted (the runtime tap skips it, ADR-0020) —
    // only the child's own `SessionStarted`/turn events land in the shared log,
    // exactly as the persistence tap would record them into the root file.
    log.extend(record_turn(&mut sub, &child, None).await);

    // Both sessions are torn down together (mirrors a process restart after a
    // crash — nothing survives in memory, only the log does).
    holly.hibernate(parent.clone()).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == parent),
    )
    .await;
    // The child hibernates too (cascade); drain its event so it doesn't leak
    // into a later `recv_until` on an unrelated predicate.
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionHibernated { session, .. } if *session == child),
    )
    .await;

    // Resuming only the parent id must still bring the child back to life.
    holly.resume(parent.clone(), log).await.unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == parent),
    )
    .await;
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionStarted { session, .. } if *session == child),
    )
    .await;

    // `ListSessions` must show the child live again, with its real parent —
    // not merely absent (never touched) or present-but-rootless (the lazy
    // blank-respawn `holly.rs` falls back to for an unknown live id, which
    // would show `parent: None` because the cascade never re-registered the
    // `parent_links` edge hibernation tore down).
    let corr = "q".to_string();
    holly
        .send(InMsg::ListSessions {
            correlation_id: corr.clone(),
        })
        .await
        .unwrap();
    let ev = recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::SessionList { correlation_id, .. } if *correlation_id == corr),
    )
    .await;
    let OutEvent::SessionList { sessions, .. } = ev else {
        unreachable!()
    };
    let child_info = sessions
        .iter()
        .find(|i| i.session == child)
        .expect("resumed child must be listed live, not silently dropped");
    assert_eq!(
        child_info.parent,
        Some(parent.clone()),
        "cascade must re-register the parent_links edge, not just the bare id"
    );

    // The child is reachable and continuable, not a dead id that silently
    // swallows every routed message.
    holly
        .send(InMsg::prompt(child.clone(), "continue"))
        .await
        .unwrap();
    recv_until(
        &mut sub,
        |e| matches!(e, OutEvent::Done { session, .. } if *session == child),
    )
    .await;
    assert!(
        !seen.lock().unwrap().is_empty(),
        "the child's provider must have been called at least once post-resume"
    );
}
