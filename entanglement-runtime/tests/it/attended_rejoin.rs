//! A card that waits with no deadline (ADR-0247) stays answerable on every
//! in-process rejoin path: the watching head leaves and another reattaches,
//! the session hibernates and resumes, or the process restarts and resumes
//! it. The `serve` reconnect path is `attended_serve`.

use std::time::Duration;

use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::attendance::Attendance;
use tokio::sync::broadcast::Receiver;

use crate::attended_mode::{approve, asking, bash_engine, card, ran};
use crate::unattended_mode::{bash_call, collect, done_text};

type Log = Vec<(Option<InMsg>, OutEvent)>;

/// Prompt `sid` on `holly` and record its events through its first card.
async fn park(holly: &Holly, sid: &SessionId) -> (String, Log) {
    let mut sub = holly.subscribe();
    let prompt = InMsg::prompt(sid.clone(), "go");
    holly.send(prompt.clone()).await.unwrap();
    let mut log: Log = Vec::new();
    let mut inbound = Some(prompt);
    loop {
        let ev = next_of(&mut sub, sid).await;
        let rid = match &ev {
            OutEvent::ToolRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        };
        log.push((inbound.take(), ev));
        if let Some(rid) = rid {
            return (rid, log);
        }
    }
}

pub(crate) async fn next_of(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> OutEvent {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("an event arrives")
            .expect("the engine is up");
        if ev.session() == Some(sid) {
            return ev;
        }
    }
}

/// Approve `rid` on `sid` and assert the call ran.
async fn approve_runs(holly: &Holly, sid: &SessionId, rid: &str) {
    let after = holly.subscribe();
    holly.send(approve(sid, rid)).await.unwrap();
    let events = collect(after, sid).await;
    assert!(ran(&events, "echo hi"), "{events:?}");
}

pub(crate) fn engine(attendance: Attendance) -> Holly {
    bash_engine(
        asking(1),
        vec![bash_call("t1", "echo hi"), done_text()],
        attendance,
    )
}

/// The watcher leaves; the card outlives `question_timeout`, is still
/// listed, and a reattached head's approve runs it.
#[tokio::test]
async fn a_card_outlives_its_watcher_and_a_reattached_head_answers_it() {
    let attendance = Attendance::new();
    let watcher = attendance.watcher();
    let sid = SessionId::new("s1");
    watcher.attend(&sid);
    let holly = engine(attendance.clone());
    let (rid, _) = park(&holly, &sid).await;
    drop(watcher);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::ListPending {
            correlation_id: "c".into(),
            root: None,
        })
        .await
        .unwrap();
    let listed = loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::PendingList { approvals, .. })) => break approvals,
            Ok(Ok(_)) => {}
            other => panic!("expected a PendingList: {other:?}"),
        }
    };
    assert!(listed.iter().any(|a| a.request_id == rid), "{listed:?}");
    let reattached = attendance.watcher();
    reattached.attend(&sid);
    approve_runs(&holly, &sid, &rid).await;
}

/// Hibernate ends the parked card; resume re-offers it, and the re-offered
/// card waits past the timeout too.
#[tokio::test]
async fn a_hibernated_sessions_card_is_reoffered_on_resume_and_waits() {
    let attendance = Attendance::new();
    let _tui = attendance.watch_all();
    let holly = engine(attendance);
    let sid = SessionId::new("s1");
    let (_, log) = park(&holly, &sid).await;
    let mut sub = holly.subscribe();
    holly.hibernate(sid.clone()).await.unwrap();
    while !matches!(
        next_of(&mut sub, &sid).await,
        OutEvent::SessionHibernated { .. }
    ) {}

    holly.resume(sid.clone(), log).await.unwrap();
    let rid = card(&mut sub, &sid).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    approve_runs(&holly, &sid, &rid).await;
}

/// A restart (a fresh engine, e.g. the TUI relaunched) resumes the log and
/// re-offers the card, which waits for its approver.
#[tokio::test]
async fn a_restarted_process_reoffers_the_card_on_resume() {
    let sid = SessionId::new("s1");
    let first = Attendance::new();
    let _first_tui = first.watch_all();
    let (_, log) = park(&engine(first), &sid).await;

    let second = Attendance::new();
    let _second_tui = second.watch_all();
    let holly = engine(second);
    let mut sub = holly.subscribe();
    holly.resume(sid.clone(), log).await.unwrap();
    let rid = card(&mut sub, &sid).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    approve_runs(&holly, &sid, &rid).await;
}

/// A card that waited for an attended session keeps waiting once
/// re-offered after a hibernate, though nobody watches any more.
#[tokio::test]
async fn a_waived_card_is_reoffered_still_waiting_with_no_watcher() {
    let attendance = Attendance::new();
    let watcher = attendance.watcher();
    let sid = SessionId::new("s1");
    watcher.attend(&sid);
    let holly = engine(attendance);
    let (_, log) = park(&holly, &sid).await;
    drop(watcher);
    let mut sub = holly.subscribe();
    holly.hibernate(sid.clone()).await.unwrap();
    while !matches!(
        next_of(&mut sub, &sid).await,
        OutEvent::SessionHibernated { .. }
    ) {}

    holly.resume(sid.clone(), log).await.unwrap();
    let rid = card(&mut sub, &sid).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    approve_runs(&holly, &sid, &rid).await;
}

/// A session's end drops its waivers (a hibernate keeps them): nothing can
/// re-offer a closed session's call.
#[tokio::test]
async fn a_closed_session_forgets_its_waivers() {
    use entanglement_runtime::attendance::Expiry;
    let attendance = Attendance::new();
    let watcher = attendance.watcher();
    let sid = SessionId::new("s1");
    watcher.attend(&sid);
    let holly = engine(attendance.clone());
    let (rid, _) = park(&holly, &sid).await;
    drop(watcher);
    let secs = || {
        Expiry::unless_attended(
            Some(Duration::from_secs(2)),
            &attendance,
            std::slice::from_ref(&sid),
            &rid,
        )
        .secs()
    };
    assert_eq!(secs(), 0, "the attended park was waived");
    holly
        .send(InMsg::CloseSession {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while secs() != 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the end never forgot the waiver"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
