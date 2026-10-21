//! A resume right behind a hibernate (#884 review, ADR-0239). Since
//! ADR-0244 the resumed task B is started at once and waits for the
//! hibernating task A's exit before it registers or emits; this keeps
//! covering the outcome: B is live and its sequence continues above
//! everything A and the runtime used.

use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId,
};
use std::sync::Arc;
use tokio::sync::broadcast::Receiver;

const WAIT: Duration = Duration::from_secs(5);

/// Every round takes a while, so a hibernate lands mid-stream.
struct SlowLlm;

#[async_trait]
impl Llm for SlowLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(stream_from_response(LlmResponse {
            text: "ok".into(),
            ..Default::default()
        }))
    }
}

async fn next_for(sub: &mut Receiver<OutEvent>, sid: &SessionId, max: &mut u64) -> OutEvent {
    loop {
        let ev = tokio::time::timeout(WAIT, sub.recv())
            .await
            .expect("timed out")
            .expect("event stream open");
        if ev.session() == Some(sid) {
            *max = (*max).max(ev.seq().unwrap_or(0));
            return ev;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_right_behind_a_hibernate_stays_live_and_unique() {
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(SlowLlm) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let mut sub = holly.subscribe();
    let sid = SessionId::new("race");
    let mut max = 0;

    // Turn one, recorded as the resume log.
    let prompt = InMsg::prompt(sid.clone(), "one");
    holly.send(prompt.clone()).await.unwrap();
    let (mut log, mut paired) = (Vec::new(), Some(prompt));
    loop {
        let ev = next_for(&mut sub, &sid, &mut max).await;
        let done = matches!(ev, OutEvent::Done { .. });
        log.push((paired.take(), ev));
        if done {
            break;
        }
    }

    // Turn two is mid-stream when the hibernate and the resume go in back
    // to back, while A is still unwinding.
    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    let (mut hibernated, mut started) = (false, false);
    while !(hibernated && started) {
        match next_for(&mut sub, &sid, &mut max).await {
            OutEvent::SessionHibernated { .. } => hibernated = true,
            OutEvent::SessionStarted { .. } => started = true,
            _ => {}
        }
    }
    // B is the live task: A's retire never unregisters it.
    assert!(holly.is_live(&sid), "A's retire must not unregister B");

    // A's `SessionHibernated` is its last event and B authors nothing before a
    // prompt, so `max` already covers every seq either task used.
    let before = max;
    holly.emit_for_session(&sid, |seq| OutEvent::Error {
        session: sid.clone(),
        seq,
        message: "runtime".into(),
    });
    let emitted = loop {
        match next_for(&mut sub, &sid, &mut max).await {
            OutEvent::Error { seq, message, .. } if message == "runtime" => break seq,
            _ => {}
        }
    };
    assert!(
        emitted > before,
        "runtime seq {emitted} must exceed {before}"
    );

    holly
        .send(InMsg::prompt(sid.clone(), "three"))
        .await
        .unwrap();
    let done = loop {
        if let OutEvent::Done { seq, .. } = next_for(&mut sub, &sid, &mut max).await {
            break seq;
        }
    };
    assert!(done > emitted, "B's turn {done} continues past {emitted}");
}
