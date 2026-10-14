//! A graded spawn parked on its approval card (#849, ADR-0232): the card can
//! wait as long as the user takes, so the child's answer must survive a
//! long park, a rejected card must give its spawn slot back, and a session
//! that ends must unwind the card instead of pinning the slot forever.

use std::time::Duration;

use entanglement_core::{
    AgentState, ApprovalScope, Holly, InMsg, OutEvent, PendingApproval, SessionId,
};
use entanglement_runtime::mode::Limits;
use entanglement_runtime::tool_names::AGENT_TOOL;
use tokio::sync::broadcast::Receiver;

use crate::spawn_grading::{spawn_calls, table_with};

const BLOCKING: &str = r#"{"agent":"general","prompt":"child-task"}"#;
const WAIT: Duration = Duration::from_secs(5);

/// `prompt: [agent]`, so every spawn parks a card first.
fn asking_engine(spawns: usize, limits: Limits) -> Holly {
    let calls = vec![(AGENT_TOOL, BLOCKING); spawns];
    spawn_calls(&calls, table_with(&[], &[AGENT_TOOL], limits))
}

async fn next_card(sub: &mut Receiver<OutEvent>) -> String {
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                request_id, tool, ..
            })) if tool == AGENT_TOOL => return request_id,
            Ok(Ok(_)) => {}
            other => panic!("expected an `agent` approval card: {other:?}"),
        }
    }
}

async fn answer(holly: &Holly, sid: &SessionId, request_id: String, approve: bool) {
    let msg = if approve {
        InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: ApprovalScope::Once,
            mode: None,
            run_all: false,
        }
    } else {
        InMsg::Reject {
            session: sid.clone(),
            request_id,
            reason: Some("not now".into()),
        }
    };
    holly.send(msg).await.unwrap();
}

/// The parent's `agent` results, in order, up to its `Done`.
async fn agent_outputs(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> Vec<String> {
    let mut outs = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await {
        match ev {
            OutEvent::ToolOutput {
                session,
                tool,
                output,
                ..
            } if &session == sid && tool == AGENT_TOOL => outs.push(output),
            OutEvent::Done { session, .. } if &session == sid => break,
            _ => {}
        }
    }
    outs
}

async fn pending(holly: &Holly, correlation: &str) -> Vec<PendingApproval> {
    let mut sub = holly.subscribe();
    holly
        .send_from_wire(InMsg::ListPending {
            correlation_id: correlation.into(),
            root: None,
        })
        .await
        .expect("ListPending is wire-allowed");
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::PendingList {
                correlation_id,
                approvals,
            })) if correlation_id == correlation => return approvals,
            Ok(Ok(_)) => {}
            other => panic!("expected a PendingList reply: {other:?}"),
        }
    }
}

/// The child's receiver is taken after the card is answered: a burst past
/// the outbox capacity while the card waits must not cost the answer.
#[tokio::test]
async fn a_card_answered_after_an_outbox_burst_still_gets_the_childs_answer() {
    let holly = asking_engine(1, Limits::default());
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let request_id = next_card(&mut sub).await;

    let noise = SessionId::new("noise");
    for _ in 0..1500 {
        holly.emit_status(&noise, AgentState::Thinking);
    }
    let mut sub = holly.subscribe();
    answer(&holly, &sid, request_id, true).await;
    let outs = agent_outputs(&mut sub, &sid).await;
    assert!(
        outs.iter().any(|o| o.contains("child-answer")),
        "the blocking spawn must fold back the child's answer: {outs:?}"
    );
}

/// `max_agents: 1`: a rejected card frees its slot, so the next spawn runs.
#[tokio::test]
async fn a_rejected_card_releases_its_spawn_slot() {
    let limits = Limits {
        max_agents: Some(1),
        ..Limits::default()
    };
    let holly = asking_engine(2, limits);
    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let first = next_card(&mut sub).await;
    answer(&holly, &sid, first, false).await;
    // A leaked slot would refuse the second spawn before it ever asks.
    let second = next_card(&mut sub).await;
    answer(&holly, &sid, second, true).await;
    let outs = agent_outputs(&mut sub, &sid).await;
    assert!(
        outs.iter().any(|o| o.contains("child-answer")),
        "the second spawn must get the freed slot: {outs:?}"
    );
}

/// A card nobody can answer any more is unwound when its session ends or
/// hibernates (a resume re-offers the parked call, which parks afresh).
#[tokio::test]
async fn ending_or_hibernating_the_session_unwinds_a_parked_spawn_card() {
    for hibernate in [false, true] {
        let holly = asking_engine(1, Limits::default());
        let sid = SessionId::new("s1");
        let mut sub = holly.subscribe();
        holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
        next_card(&mut sub).await;
        assert_eq!(pending(&holly, "before").await.len(), 1);

        let msg = if hibernate {
            InMsg::HibernateSession {
                session: sid.clone(),
            }
        } else {
            InMsg::CloseSession {
                session: sid.clone(),
            }
        };
        holly.send(msg).await.unwrap();
        loop {
            match tokio::time::timeout(WAIT, sub.recv()).await {
                Ok(Ok(OutEvent::SessionEnded { session, .. }))
                | Ok(Ok(OutEvent::SessionHibernated { session, .. }))
                    if session == sid =>
                {
                    break
                }
                Ok(Ok(_)) => {}
                other => panic!("expected the session to end: {other:?}"),
            }
        }
        // The runtime folds the lifecycle event on its own subscriber; poll
        // briefly instead of racing it.
        let mut left = pending(&holly, "after").await;
        for _ in 0..50 {
            if left.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            left = pending(&holly, "after").await;
        }
        assert!(left.is_empty(), "hibernate={hibernate}: {left:?}");
    }
}
