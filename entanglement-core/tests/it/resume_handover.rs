//! What an old task hands its successor survives the edges (#906,
//! ADR-0246): a successor hibernated before it ran passes it on, a dropped
//! tail still leaves the final state, and a `Hibernate` stashed behind a
//! mid-stream `Pause` hibernates.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolOverlayEntry,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;

use super::seq_robustness::{next_for, record_turn, slow_reverter};

fn reverting_engine() -> Holly {
    Holly::spawn(EngineConfig {
        file_reverter: Some(slow_reverter()),
        ..EngineConfig::default()
    })
}

/// Busy `sid` in a slow revert and queue a deny behind it.
async fn deny_during_teardown(holly: &Holly, sid: &SessionId) {
    let revert = InMsg::RevertFiles {
        session: sid.clone(),
        to_turn: None,
    };
    holly.send(revert).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let deny = InMsg::SetToolOverlay {
        session: sid.clone(),
        entries: vec![ToolOverlayEntry::deny("bash")],
    };
    holly.send(deny).await.unwrap();
}

/// The overlay the resumed `sid` re-announces at start, if any; lag-safe.
async fn started_overlay(
    sub: &mut Receiver<OutEvent>,
    sid: &SessionId,
) -> Option<Vec<ToolOverlayEntry>> {
    let (mut started, mut overlay) = (false, None);
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match sub.recv().await {
                Ok(OutEvent::SessionStarted { session, .. }) if &session == sid => started = true,
                Ok(OutEvent::ToolOverlayChanged { session, entries })
                    if &session == sid && started =>
                {
                    overlay = Some(entries)
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            }
        }
    })
    .await;
    assert!(started, "the resumed task started");
    overlay
}

/// A takes a deny during its teardown and stays busy after it (a second
/// slow revert). B, resumed behind A, is hibernated again only after A
/// emitted the deny, so B's own tap never sees it, and before B ran. C,
/// resumed while A still tears down, waits on B, which passes A's hand-over
/// on: C carries the deny.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_double_hibernate_keeps_the_teardown_deny() {
    let holly = reverting_engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("double");
    let log = record_turn(&holly, &mut sub, &sid).await;
    deny_during_teardown(&holly, &sid).await;
    let revert = InMsg::RevertFiles {
        session: sid.clone(),
        to_turn: None,
    };
    holly.send(revert).await.unwrap();
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log.clone()).await.unwrap();
    while !matches!(
        next_for(&mut sub, &sid).await,
        OutEvent::ToolOverlayChanged { .. }
    ) {}
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();

    let (mut started, mut overlay, mut hibernated) = (false, None, 0);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match next_for(&mut sub, &sid).await {
                OutEvent::SessionStarted { .. } => started = true,
                OutEvent::ToolOverlayChanged { entries, .. } if started => overlay = Some(entries),
                OutEvent::SessionHibernated { .. } => hibernated += 1,
                _ => {}
            }
        }
    })
    .await;
    assert!(started);
    assert_eq!(hibernated, 1, "only A ran and hibernated; B never ran");
    assert_eq!(
        overlay,
        Some(vec![ToolOverlayEntry::deny("bash")]),
        "deny lost on C"
    );
}

/// A flood for the id during the teardown overflows the tail, so it is
/// dropped whole; the final state still carries the deny A took.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_tail_still_hands_over_the_teardown_deny() {
    let holly = reverting_engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("flood");
    let log = record_turn(&holly, &mut sub, &sid).await;
    deny_during_teardown(&holly, &sid).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    // The waiting successor's inbox fills, and each shed frame is an
    // `Error` for the id inside A's teardown window.
    for i in 0..1200 {
        let filler = InMsg::SetSessionMeta {
            session: sid.clone(),
            name: Some(format!("n{i}")),
            action: None,
            if_unset: false,
        };
        holly.send(filler).await.unwrap();
    }
    assert_eq!(
        started_overlay(&mut sub, &sid).await,
        Some(vec![ToolOverlayEntry::deny("bash")]),
        "the deny reached the successor through its final state"
    );
}

struct SlowLlm;

#[async_trait]
impl Llm for SlowLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        tokio::time::sleep(Duration::from_millis(600)).await;
        Ok(stream_from_response(LlmResponse {
            text: "r".into(),
            ..Default::default()
        }))
    }
}

/// `Pause` then `HibernateSession` while a round streams: both are stashed,
/// the hold waits on a closed inbox, and the stashed `Hibernate` still
/// hibernates the session rather than ending it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_then_hibernate_mid_stream_hibernates() {
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(SlowLlm) as Box<dyn Llm>),
        ..EngineConfig::default()
    });
    let mut sub = holly.subscribe();
    let sid = SessionId::new("held");
    holly.send(InMsg::prompt(sid.clone(), "x")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let pause = InMsg::PauseSession {
        session: sid.clone(),
    };
    holly.send(pause).await.unwrap();
    holly.hibernate(sid.clone()).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let ev = next_for(&mut sub, &sid).await;
            if matches!(
                ev,
                OutEvent::SessionHibernated { .. } | OutEvent::SessionEnded { .. }
            ) {
                return ev;
            }
        }
    })
    .await
    .expect("the session ended somehow");
    assert!(
        matches!(end, OutEvent::SessionHibernated { .. }),
        "got {end:?}"
    );
}
