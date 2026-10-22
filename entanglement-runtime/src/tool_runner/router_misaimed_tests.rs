//! A decision of the wrong kind for its card (#895, ADR-0245): the router
//! refuses it with an `Error` on the addressed session, and the card stays
//! parked — never the silent unwind that left the turn waiting forever. A
//! decision for no card at all is refused the same way (ADR-0247).

use std::time::Duration;

use entanglement_core::{ApprovalScope, EngineConfig, Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::{error::TryRecvError, Receiver};

use super::tests::{router, setup};
use crate::pending::test_support::spawn_session;
use crate::seam::Decision;

/// The `(session, message)` of each `Error` and the session of each
/// `ToolRequest` emitted since `sub` subscribed.
fn drain(sub: &mut Receiver<OutEvent>) -> (Vec<(SessionId, String)>, Vec<SessionId>) {
    let (mut errors, mut cards) = (Vec::new(), Vec::new());
    loop {
        match sub.try_recv() {
            Ok(OutEvent::Error {
                session, message, ..
            }) => errors.push((session, message)),
            Ok(OutEvent::ToolRequest { session, .. }) => cards.push(session),
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(_) => return (errors, cards),
        }
    }
}

/// A `RetractQuestion` aimed at a background script's card, sent on the
/// compaction successor before any fold moved it: the heal moves and
/// announces the card (the frame answers none), the sender gets an `Error`,
/// and the card still takes an `Approve`.
#[tokio::test]
async fn a_retract_aimed_at_an_approval_card_is_refused_and_the_card_stays() {
    let (holly, r, succ, mut rx) = setup().await;
    let mut sub = holly.subscribe();
    let rid = "x-1:rhai:edit";
    r.route(InMsg::RetractQuestion {
        session: succ.clone(),
        request_id: rid.into(),
    });
    let (errors, cards) = drain(&mut sub);
    assert_eq!(
        cards,
        std::slice::from_ref(&succ),
        "the moved card is shown"
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, succ);
    assert!(
        errors[0].1.contains("`RetractQuestion` refused")
            && errors[0].1.contains(rid)
            && errors[0].1.contains("answer it with `Approve` or `Reject`"),
        "{}",
        errors[0].1
    );
    assert!(rx.try_recv().is_err(), "the card is still parked");
    assert_eq!(r.pending.snapshot(None)[0].session, succ, "still listed");
    r.route(InMsg::Approve {
        session: succ,
        request_id: rid.into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    });
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx).await,
        Ok(Ok(Decision::Approve { .. }))
    ));
}

/// The reverse: an `Approve` aimed at an `ask_user` question is refused,
/// and the question still takes its answer.
#[tokio::test]
async fn an_approve_aimed_at_a_question_is_refused_and_the_question_stays() {
    let holly = Holly::spawn(EngineConfig::default());
    let s = SessionId::new("s");
    spawn_session(&holly, &s, None, None).await;
    let r = router(&holly);
    let mut rx = r.pending.register(&s, "q", "ask_user", "q");
    let mut sub = holly.subscribe();
    r.route(InMsg::Approve {
        session: s.clone(),
        request_id: "q".into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    });
    let (errors, _) = drain(&mut sub);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].1.contains("`Approve` refused") && errors[0].1.contains("`AnswerQuestion`"),
        "{}",
        errors[0].1
    );
    assert!(rx.try_recv().is_err(), "the question is still parked");
    r.route(InMsg::answer_question(s, "q", vec![vec!["a".into()]]));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx).await,
        Ok(Ok(Decision::Answer { .. }))
    ));
}

/// A decision for an id nothing holds is refused with an `Error` (ADR-0247):
/// a late answer must not look like it took effect.
#[tokio::test]
async fn a_decision_for_an_unknown_id_is_refused() {
    let holly = Holly::spawn(EngineConfig::default());
    let s = SessionId::new("s");
    spawn_session(&holly, &s, None, None).await;
    let r = router(&holly);
    let mut sub = holly.subscribe();
    r.route(InMsg::RetractQuestion {
        session: s.clone(),
        request_id: "gone".into(),
    });
    r.route(InMsg::Reject {
        session: s.clone(),
        request_id: "gone".into(),
        reason: None,
    });
    let (errors, _) = drain(&mut sub);
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors[0]
            .1
            .contains("`RetractQuestion` refused: no open question `gone`"),
        "{}",
        errors[0].1
    );
    assert!(
        errors[1]
            .1
            .contains("`Reject` refused: no pending card `gone`"),
        "{}",
        errors[1].1
    );
    assert!(errors.iter().all(|(session, _)| *session == s));
}

/// Two heads approve one card: the first takes it with no `Error`; the
/// second's late approve learns it had no effect.
#[tokio::test]
async fn a_second_heads_late_approve_is_refused() {
    let holly = Holly::spawn(EngineConfig::default());
    let s = SessionId::new("s");
    spawn_session(&holly, &s, None, None).await;
    let r = router(&holly);
    let rx = r.pending.register(&s, "c", "tool", "bash");
    let mut sub = holly.subscribe();
    let approve = || InMsg::Approve {
        session: s.clone(),
        request_id: "c".into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    };
    r.route(approve());
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx).await,
        Ok(Ok(Decision::Approve { .. }))
    ));
    assert!(
        drain(&mut sub).0.is_empty(),
        "the first approve is delivered"
    );
    r.route(approve());
    let (errors, _) = drain(&mut sub);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0]
            .1
            .contains("`Approve` refused: no pending card `c`"),
        "{}",
        errors[0].1
    );
}
