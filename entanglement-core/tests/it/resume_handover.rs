//! What an old task hands its successor survives a double hibernate (#906,
//! ADR-0246): a successor hibernated before it ran passes the hand-over on.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};

use super::seq_one_task::start_slow_revert;
use super::seq_robustness::{config, next_for, record_turn, slow_reverter};

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

/// A takes a `SetMode research` during its teardown and stays busy after it
/// (a second slow revert). B, resumed behind A, is hibernated again only
/// after A emitted the switch, so B's own tap never sees it, and before B
/// ran. C, resumed while A still tears down, waits on B, which passes A's
/// hand-over on: C starts in `research`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_double_hibernate_keeps_the_teardown_mode() {
    let holly = reverting_engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("double");
    let log = record_turn(&holly, &mut sub, &sid).await;
    start_slow_revert(&holly, &sid).await;
    let narrow = InMsg::SetMode {
        session: sid.clone(),
        mode: "research".into(),
    };
    holly.send(narrow).await.unwrap();
    let revert = InMsg::RevertFiles {
        session: sid.clone(),
        to_turn: None,
    };
    holly.send(revert).await.unwrap();
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log.clone()).await.unwrap();
    while !matches!(next_for(&mut sub, &sid).await, OutEvent::ModeChanged { .. }) {}
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();

    let (mut started, mut mode, mut hibernated) = (false, None, 0);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match next_for(&mut sub, &sid).await {
                OutEvent::SessionStarted { .. } => started = true,
                OutEvent::ModeChanged { mode: m, .. } if started => mode = Some(m),
                OutEvent::SessionHibernated { .. } => hibernated += 1,
                _ => {}
            }
        }
    })
    .await;
    assert!(started, "C started");
    assert_eq!(hibernated, 1, "only A ran and hibernated; B never ran");
    assert_eq!(mode.as_deref(), Some("research"), "mode lost on C");
}
