//! `ReplayFrom` → `History` late-subscriber round-trip (#160, ADR-0072).
//!
//! A head that subscribed late asks for a session's content history from a seq
//! cursor; the runtime's history responder reads the persisted log and answers
//! with a single [`OutEvent::History`] snapshot — content events past the cursor,
//! `correlation_id` echoed. Drives the responder end-to-end against a pre-written
//! log so the wiring (inbound fan-out → log read → `emit_history`) is exercised,
//! not just the pure filter.

use std::time::Duration;

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::history::spawn_history_responder;
use entanglement_runtime::session_store::{append, LogPayload, LogRecord};

fn out(sid: &SessionId, ev: OutEvent) -> LogRecord {
    LogRecord::new(sid.clone(), LogPayload::Out(ev))
}

#[tokio::test]
async fn replay_from_answers_with_content_after_cursor() {
    // A distinct cwd keys a distinct log file (session_store hashes the cwd), so
    // this test's records never collide with another run's.
    let tmp = tempfile::tempdir().expect("temp dir");
    let cwd = tmp.path().to_path_buf();
    let sid = SessionId::new("replay-root");

    // Persist a lifecycle event (no seq) plus three content events.
    for r in [
        out(
            &sid,
            OutEvent::SessionStarted {
                session: sid.clone(),
                parent: None,
                profile: "build".into(),
                model: None,
                root: true,
                ts: 0,
            },
        ),
        out(
            &sid,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 1,
                text: "a".into(),
            },
        ),
        out(
            &sid,
            OutEvent::TextDelta {
                session: sid.clone(),
                seq: 2,
                text: "b".into(),
            },
        ),
        out(
            &sid,
            OutEvent::Done {
                session: sid.clone(),
                seq: 3,
            },
        ),
    ] {
        append(&cwd, &sid, &r).expect("append record");
    }

    let holly = Holly::spawn(EngineConfig::default());
    let mut sub = holly.subscribe();
    spawn_history_responder(&holly, cwd.clone());

    holly
        .send(InMsg::ReplayFrom {
            session: sid.clone(),
            correlation_id: "corr-1".into(),
            after_seq: 1,
        })
        .await
        .expect("send ReplayFrom");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("timed out waiting for History")
            .expect("broadcast closed");
        if let OutEvent::History {
            correlation_id,
            session,
            events,
        } = ev
        {
            assert_eq!(correlation_id, "corr-1", "the request token is echoed back");
            assert_eq!(session, sid);
            // after_seq = 1 drops the SessionStarted (no seq) and the seq-1 delta,
            // keeping the seq-2 delta and the seq-3 Done in log order.
            assert_eq!(events.len(), 2, "got {events:?}");
            assert!(matches!(events[0], OutEvent::TextDelta { seq: 2, .. }));
            assert!(matches!(events[1], OutEvent::Done { seq: 3, .. }));
            break;
        }
    }
}
