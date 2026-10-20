//! A decision of the wrong kind for its card, through the real executor and
//! the untrusted wire path (#895, ADR-0245): the sender gets an `Error`, the
//! card stays parked and listed, and its own decision still finishes the turn.

use std::time::Duration;

use entanglement_core::{ApprovalOutcome, Holly, InMsg, OutEvent, Permission, SessionId};
use entanglement_runtime::mode::Limits;
use entanglement_runtime::tool_names::ASK_USER_TOOL;
use entanglement_runtime::ToolRegistry;
use tokio::sync::broadcast::Receiver;

use crate::unattended_mode::{
    ask_user_call, bash_call, done_text, one_mode_table, spawn_with_policy, EchoBash,
};

/// An engine whose mode asks for every `bash` call, scripted with `calls`.
fn engine(calls: Vec<entanglement_core::LlmResponse>) -> Holly {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_with_policy(
        reg,
        one_mode_table(Permission::Ask, Limits::default()),
        calls,
    )
}

/// Wait on `sub` for the first event `pick` maps, panicking on any event
/// `forbid` names first.
async fn next<T>(
    sub: &mut Receiver<OutEvent>,
    mut pick: impl FnMut(&OutEvent) -> Option<T>,
    forbid: impl Fn(&OutEvent) -> bool,
) -> T {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(ev)) => {
                assert!(!forbid(&ev), "unexpected {ev:?}");
                if let Some(found) = pick(&ev) {
                    return found;
                }
            }
            other => panic!("the awaited event never came: {other:?}"),
        }
    }
}

fn error_on(sid: &SessionId) -> impl FnMut(&OutEvent) -> Option<String> + '_ {
    move |ev| match ev {
        OutEvent::Error {
            session, message, ..
        } if session == sid => Some(message.clone()),
        _ => None,
    }
}

/// The turn's end, or the card's: neither may come before its decision.
fn ends(ev: &OutEvent) -> bool {
    matches!(
        ev,
        OutEvent::ApprovalResolved { .. } | OutEvent::Done { .. } | OutEvent::ToolOutput { .. }
    )
}

#[tokio::test]
async fn a_wire_retract_aimed_at_an_approval_card_is_refused() {
    let holly = engine(vec![bash_call("t1", "ls"), done_text()]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let rid = next(
        &mut sub,
        |ev| match ev {
            OutEvent::ToolRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        },
        |_| false,
    )
    .await;

    holly
        .send_from_wire(InMsg::RetractQuestion {
            session: sid.clone(),
            request_id: rid.clone(),
        })
        .await
        .expect("RetractQuestion is wire-allowed");
    let message = next(&mut sub, error_on(&sid), ends).await;
    assert!(message.contains("`RetractQuestion` refused"), "{message}");

    holly
        .send_from_wire(InMsg::ListPending {
            correlation_id: "c".into(),
            root: None,
        })
        .await
        .expect("ListPending is wire-allowed");
    let listed = next(
        &mut sub,
        |ev| match ev {
            OutEvent::PendingList { approvals, .. } => Some(approvals.clone()),
            _ => None,
        },
        ends,
    )
    .await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_id, rid, "the card is still listed");

    holly
        .send_from_wire(InMsg::Approve {
            session: sid.clone(),
            request_id: rid.clone(),
            scope: Default::default(),
            mode: None,
            run_all: false,
        })
        .await
        .expect("Approve is wire-allowed");
    let outcome = next(
        &mut sub,
        |ev| match ev {
            OutEvent::ApprovalResolved {
                request_id,
                outcome,
                ..
            } if request_id == &rid => Some(*outcome),
            _ => None,
        },
        |_| false,
    )
    .await;
    assert_eq!(outcome, ApprovalOutcome::Approved);
    next(
        &mut sub,
        |ev| matches!(ev, OutEvent::Done { session, .. } if session == &sid).then_some(()),
        |_| false,
    )
    .await;
}

#[tokio::test]
async fn a_wire_approve_aimed_at_a_question_is_refused() {
    let input = r#"{"questions":[{"question":"Which?","options":[{"label":"A"}]}]}"#;
    let holly = engine(vec![ask_user_call("q1", input), done_text()]);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let rid = next(
        &mut sub,
        |ev| match ev {
            OutEvent::UserQuestion { request_id, .. } => Some(request_id.clone()),
            _ => None,
        },
        |_| false,
    )
    .await;

    holly
        .send_from_wire(InMsg::Approve {
            session: sid.clone(),
            request_id: rid.clone(),
            scope: Default::default(),
            mode: None,
            run_all: false,
        })
        .await
        .expect("Approve is wire-allowed");
    let message = next(&mut sub, error_on(&sid), ends).await;
    assert!(message.contains("`Approve` refused"), "{message}");

    holly
        .send_from_wire(InMsg::ListQuestions {
            correlation_id: "c".into(),
            session: Some(sid.clone()),
        })
        .await
        .expect("ListQuestions is wire-allowed");
    let open = next(
        &mut sub,
        |ev| match ev {
            OutEvent::QuestionList { questions, .. } => Some(questions.clone()),
            _ => None,
        },
        ends,
    )
    .await;
    assert_eq!(open.len(), 1, "the question is still open: {open:?}");

    holly
        .send_from_wire(InMsg::answer_question(
            sid.clone(),
            rid,
            vec![vec!["A".into()]],
        ))
        .await
        .expect("AnswerQuestion is wire-allowed");
    let output = next(
        &mut sub,
        |ev| match ev {
            OutEvent::ToolOutput { tool, output, .. } if tool == ASK_USER_TOOL => {
                Some(output.clone())
            }
            _ => None,
        },
        |_| false,
    )
    .await;
    assert_eq!(output, "A");
}
