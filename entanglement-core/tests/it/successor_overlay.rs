//! A compaction successor keeps its source's per-session tool overlay (#776):
//! a user's `SetToolOverlay` enable/deny survives every fork path (ADR-0205)
//! and replays, unless a live mode change already dropped it — the overlay is
//! mode-scoped (ADR-0207). Carrying it never changes the advertised tools
//! array (ADR-0196/0204: the overlay is consulted at dispatch only).
//! Scaffolding is shared with `successor_model`.

use entanglement_core::session::Session;
use entanglement_core::{Holly, InMsg, OutEvent, SessionId, ToolOverlayEntry};
use tokio::sync::broadcast::Receiver;

use super::successor_model::{compact, next, start_and_switch, Harness, Path};

fn overlay() -> Vec<ToolOverlayEntry> {
    vec![
        ToolOverlayEntry::allow("bash"),
        ToolOverlayEntry::deny("probe"),
    ]
}

/// Set `entries` on `sid` and wait for the ack.
async fn set_overlay(holly: &Holly, sub: &mut Receiver<OutEvent>, sid: &SessionId) {
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries: overlay(),
        })
        .await
        .unwrap();
    while !matches!(next(sub).await, OutEvent::ToolOverlayChanged { session, .. } if session == *sid)
    {
    }
}

fn logged_overlays(events: &[OutEvent]) -> Vec<Vec<ToolOverlayEntry>> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOverlayChanged { entries, .. } => Some(entries.clone()),
            _ => None,
        })
        .collect()
}

fn replayed_overlay(
    cfg: &entanglement_core::EngineConfig,
    sid: &SessionId,
    log: &[OutEvent],
) -> Vec<ToolOverlayEntry> {
    let records: Vec<(Option<InMsg>, OutEvent)> = log.iter().cloned().map(|e| (None, e)).collect();
    Session::replay(&records, cfg, sid)
        .expect("replay")
        .tool_overlay
}

#[tokio::test]
async fn every_compaction_successor_keeps_the_overlay() {
    for path in Path::ALL {
        let h = Harness::new();
        let cfg = h.config(path.auto_compact());
        let holly = Holly::spawn(cfg.clone());
        let sid = SessionId::new("s1");
        let mut sub = holly.subscribe();
        start_and_switch(&holly, &mut sub, &sid).await;
        set_overlay(&holly, &mut sub, &sid).await;
        let source_tools = h.tools.lock().unwrap().last().cloned();

        let (successor, events) = compact(&holly, &mut sub, &sid, path).await;

        assert_eq!(logged_overlays(&events), [overlay()], "{path:?}");
        // After the successor's `ModeChanged`, so neither replay nor the
        // runtime's mirror reads it as dropped by a mode switch.
        let mode_at = events
            .iter()
            .position(|e| matches!(e, OutEvent::ModeChanged { .. }));
        let overlay_at = events
            .iter()
            .position(|e| matches!(e, OutEvent::ToolOverlayChanged { .. }));
        assert!(mode_at < overlay_at, "{path:?}: {events:?}");
        assert_eq!(replayed_overlay(&cfg, &successor, &events), overlay());
        // The enable for an unadvertised `bash` and the deny for `probe`
        // leave the tools array exactly as the source sent it.
        assert_eq!(
            h.tools.lock().unwrap().last().cloned(),
            source_tools,
            "{path:?}"
        );
    }
}

/// A live mode change drops the overlay (ADR-0207) in core too, so the
/// successor has none to inherit — and the source's own log replays the drop.
#[tokio::test]
async fn an_overlay_dropped_by_a_mode_change_is_not_carried() {
    let h = Harness::new();
    let cfg = h.config(true);
    let holly = Holly::spawn(cfg.clone());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    // A second receiver for the source's whole log, drained once it settles.
    let mut log_sub = holly.subscribe();
    start_and_switch(&holly, &mut sub, &sid).await;
    holly
        .send(InMsg::SetToolOverlay {
            session: sid.clone(),
            entries: overlay(),
        })
        .await
        .unwrap();
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "build".into(),
        })
        .await
        .unwrap();
    while !matches!(next(&mut sub).await, OutEvent::ModeChanged { session, mode } if session == sid && mode == "build")
    {
    }
    let mut source_log = Vec::new();
    while let Ok(ev) = log_sub.try_recv() {
        if ev.session() == Some(&sid) {
            source_log.push(ev);
        }
    }
    assert_eq!(logged_overlays(&source_log), [overlay()]);
    assert!(replayed_overlay(&cfg, &sid, &source_log).is_empty());

    let (successor, events) = compact(&holly, &mut sub, &sid, Path::Manual).await;
    assert!(logged_overlays(&events).is_empty(), "{events:?}");
    assert!(replayed_overlay(&cfg, &successor, &events).is_empty());
}

/// The successor's overlay survives resuming twice: each resume re-announces
/// the (same) mode after the logged overlay, which must not read as a switch.
#[tokio::test]
async fn a_twice_resumed_successor_keeps_the_overlay() {
    let h = Harness::new();
    let cfg = h.config(true);
    let holly = Holly::spawn(cfg.clone());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    start_and_switch(&holly, &mut sub, &sid).await;
    set_overlay(&holly, &mut sub, &sid).await;
    let (successor, mut log) = compact(&holly, &mut sub, &sid, Path::Manual).await;

    for _ in 0..2 {
        let fresh = Holly::spawn(cfg.clone());
        let mut fresh_sub = fresh.subscribe();
        let records = log.iter().cloned().map(|e| (None, e)).collect();
        fresh.resume(successor.clone(), records).await.unwrap();
        fresh
            .send(InMsg::prompt(successor.clone(), "again"))
            .await
            .unwrap();
        // This engine's events extend the persisted log the next resume reads.
        loop {
            let ev = next(&mut fresh_sub).await;
            if ev.session() == Some(&successor) {
                let done = matches!(ev, OutEvent::Done { .. });
                log.push(ev);
                if done {
                    break;
                }
            }
        }
        assert_eq!(replayed_overlay(&cfg, &successor, &log), overlay());
    }
}
