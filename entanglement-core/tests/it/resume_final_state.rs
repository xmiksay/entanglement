//! The old task's final mode and overlay reach its successor as they are
//! (#906, ADR-0246): a head's appended ADR-0216 `ModeChanged` must not let a
//! teardown overlay survive a clamped resume.

use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, Llm, OutEvent, SessionId, ToolOverlayEntry};
use tokio::sync::broadcast::Receiver;

use super::resume_tail::{RecordingLlm, Seen};
use super::seq_robustness::{next_for, record_turn, slow_reverter};

fn engine() -> Holly {
    let seen: Seen = Arc::default();
    Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(RecordingLlm(seen.clone())) as Box<dyn Llm>),
        file_reverter: Some(slow_reverter()),
        ..EngineConfig::default()
    })
}

/// The head's forced read-only narrowing, appended after the log.
fn forced(sid: &SessionId) -> (Option<InMsg>, OutEvent) {
    let ev = OutEvent::ModeChanged {
        session: sid.clone(),
        mode: "research".into(),
    };
    (None, ev)
}

/// Busy the old task in a slow revert, then queue an allow-bash overlay
/// behind it and hibernate.
async fn overlay_during_teardown(holly: &Holly, sid: &SessionId) {
    let revert = InMsg::RevertFiles {
        session: sid.clone(),
        to_turn: None,
    };
    holly.send(revert).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let overlay = InMsg::SetToolOverlay {
        session: sid.clone(),
        entries: vec![ToolOverlayEntry::allow("bash")],
    };
    holly.send(overlay).await.unwrap();
    holly.hibernate(sid.clone()).await.unwrap();
}

/// The resumed task's announced mode, and any overlay it re-announced.
async fn resumed(
    sub: &mut Receiver<OutEvent>,
    sid: &SessionId,
) -> (String, Option<Vec<ToolOverlayEntry>>) {
    let (mut started, mut mode, mut revived) = (false, String::new(), None);
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match next_for(sub, sid).await {
                OutEvent::SessionStarted { .. } => started = true,
                OutEvent::ModeChanged { mode: m, .. } if started => mode = m,
                OutEvent::ToolOverlayChanged { entries, .. } if started && !entries.is_empty() => {
                    revived = Some(entries)
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(started, "the resumed task started");
    (mode, revived)
}

/// The overlay was set during the teardown, after the head read its log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_teardown_overlay_does_not_survive_a_forced_mode() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("fc1");
    let mut log = record_turn(&holly, &mut sub, &sid).await;
    overlay_during_teardown(&holly, &sid).await;
    log.push(forced(&sid));
    holly.resume(sid.clone(), log).await.unwrap();
    let (mode, revived) = resumed(&mut sub, &sid).await;
    assert_eq!(mode, "research");
    assert!(revived.is_none(), "allow-bash under research: {revived:?}");
}

/// The head read its log after the overlay too, so its records hold it,
/// and the appended forced mode is their last record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_overlay_does_not_survive_a_forced_mode() {
    let holly = engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("fc2");
    let mut log = record_turn(&holly, &mut sub, &sid).await;
    overlay_during_teardown(&holly, &sid).await;
    loop {
        let ev = next_for(&mut sub, &sid).await;
        let stop = matches!(ev, OutEvent::ToolOverlayChanged { .. });
        log.push((None, ev));
        if stop {
            break;
        }
    }
    log.push(forced(&sid));
    holly.resume(sid.clone(), log).await.unwrap();
    let (mode, revived) = resumed(&mut sub, &sid).await;
    assert_eq!(mode, "research");
    assert!(revived.is_none(), "allow-bash under research: {revived:?}");
}
