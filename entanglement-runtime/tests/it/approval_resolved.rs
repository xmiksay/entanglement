//! `OutEvent::ApprovalResolved` through the real executor (#883, ADR-0240):
//! every head learns when a card leaves the runtime, whichever head (or
//! timer) removed it, always after the card's `ToolRequest`.

use std::time::Duration;

use entanglement_core::{ApprovalOutcome, Holly, InMsg, OutEvent, Permission, SessionId};
use entanglement_runtime::mode::Limits;
use entanglement_runtime::ToolRegistry;
use tokio::sync::broadcast::Receiver;

use crate::unattended_mode::{bash_call, done_text, one_mode_table, spawn_with_policy, EchoBash};

/// An engine whose mode asks for every `bash` call, scripted with `calls`.
fn engine(limits: Limits, calls: Vec<entanglement_core::LlmResponse>) -> Holly {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_with_policy(reg, one_mode_table(Permission::Ask, limits), calls)
}

/// Wait on `sub` for `sid`'s first `ToolRequest`, returning its id and seq.
async fn card(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> (String, u64) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                session,
                request_id,
                seq,
                ..
            })) if &session == sid => return (request_id, seq),
            Ok(Ok(_)) => {}
            other => panic!("expected a card on {sid}: {other:?}"),
        }
    }
}

/// Wait on `sub` for the `ApprovalResolved` of `request_id`.
async fn resolution(
    sub: &mut Receiver<OutEvent>,
    request_id: &str,
) -> (SessionId, u64, ApprovalOutcome) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ApprovalResolved {
                session,
                seq,
                request_id: rid,
                outcome,
            })) if rid == request_id => return (session, seq, outcome),
            Ok(Ok(_)) => {}
            other => panic!("expected {request_id} resolved: {other:?}"),
        }
    }
}

fn approve(sid: &SessionId, request_id: &str) -> InMsg {
    InMsg::Approve {
        session: sid.clone(),
        request_id: request_id.into(),
        scope: Default::default(),
        mode: None,
        run_all: false,
    }
}

/// Two heads on one engine: the one that did not answer learns the card is
/// gone, after the card itself, on the session that showed it.
#[tokio::test]
async fn a_card_answered_by_one_head_is_resolved_for_every_head() {
    for (decision, want) in [
        (None, ApprovalOutcome::Approved),
        (Some("no"), ApprovalOutcome::Rejected),
    ] {
        let holly = engine(Limits::default(), vec![bash_call("t1", "ls"), done_text()]);
        let sid = SessionId::new("s1");
        let mut other_head = holly.subscribe();
        let mut answering_head = holly.subscribe();
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

        let (rid, card_seq) = card(&mut answering_head, &sid).await;
        let msg = match decision {
            None => approve(&sid, &rid),
            Some(reason) => InMsg::Reject {
                session: sid.clone(),
                request_id: rid.clone(),
                reason: Some(reason.into()),
            },
        };
        holly.send(msg).await.unwrap();

        assert_eq!(card(&mut other_head, &sid).await, (rid.clone(), card_seq));
        let (session, seq, outcome) = resolution(&mut other_head, &rid).await;
        assert_eq!((session, outcome), (sid.clone(), want));
        assert!(
            seq > card_seq,
            "the end follows the card: {seq} > {card_seq}"
        );
    }
}

/// An unattended mode's timeout expires the card where no head sees it.
#[tokio::test]
async fn an_expired_card_is_resolved_as_expired() {
    let limits = Limits {
        question_timeout: 1,
        ..Limits::default()
    };
    // The first identical call is denied silently; the repeat parks.
    let calls = vec![bash_call("t1", "ls"), bash_call("t2", "ls"), done_text()];
    let holly = engine(limits, calls);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let (rid, card_seq) = card(&mut sub, &sid).await;
    let (session, seq, outcome) = resolution(&mut sub, &rid).await;
    assert_eq!((session, outcome), (sid, ApprovalOutcome::Expired));
    assert!(seq > card_seq);
}

#[tokio::test]
async fn a_stopped_card_is_resolved_as_stopped() {
    let holly = engine(Limits::default(), vec![bash_call("t1", "ls"), done_text()]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let (rid, _) = card(&mut sub, &sid).await;
    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();
    let (session, _, outcome) = resolution(&mut sub, &rid).await;
    assert_eq!((session, outcome), (sid, ApprovalOutcome::Stopped));
}

/// The end is a content event: the persistence tap logs it after its card,
/// so a resumed head's view (rebuilt from the log) drops the card too.
#[tokio::test]
async fn the_end_is_logged_after_the_card() {
    use entanglement_runtime::persistence::spawn_persistence_subscriber;
    use entanglement_runtime::session_store::{LogPayload, SessionStore};

    let tmp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::at(tmp.path());
    let holly = engine(Limits::default(), vec![bash_call("t1", "ls"), done_text()]);
    let _tap = spawn_persistence_subscriber(&holly, store.clone());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let (rid, _) = card(&mut sub, &sid).await;
    holly.send(approve(&sid, &rid)).await.unwrap();
    resolution(&mut sub, &rid).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let kinds = loop {
        let kinds: Vec<&'static str> = store
            .read(&sid)
            .expect("read log")
            .into_iter()
            .filter_map(|r| match r.payload {
                LogPayload::Out(OutEvent::ToolRequest { .. }) => Some("request"),
                LogPayload::Out(OutEvent::ApprovalResolved { outcome, .. }) => {
                    assert_eq!(outcome, ApprovalOutcome::Approved);
                    Some("resolved")
                }
                _ => None,
            })
            .collect();
        if kinds.len() == 2 {
            break kinds;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "tap never flushed: {kinds:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(kinds, ["request", "resolved"]);
}
