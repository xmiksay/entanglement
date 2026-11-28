//! ADR-0249 on the model's own call: an envelope tool (an embedder's
//! `invoke`, a host namespace's backing tool) called directly is carded,
//! listed and refused under its `call_grading` target, the name a rule and
//! an `Always` grant use — not the envelope's.

use std::time::Duration;

use entanglement_core::{ApprovalKind, InMsg, OutEvent, PendingApproval, SessionId};
use tokio::sync::broadcast::Receiver;

use super::fixture::{mode_table, spawn};
use super::{call, scripted, sid, SYSTEM};

const ENVELOPE: &str = "inventory_invoke";
const TARGET: &str = "inventory_get_stock_level";
const WAIT: Duration = Duration::from_secs(10);

/// The next event for `session` that `pick` accepts.
async fn next<T>(
    sub: &mut Receiver<OutEvent>,
    session: &SessionId,
    pick: impl Fn(&OutEvent) -> Option<T>,
) -> T {
    loop {
        let ev = tokio::time::timeout(WAIT, sub.recv())
            .await
            .expect("an event before the timeout")
            .expect("the outbox stays open");
        if ev.session().is_none_or(|s| s == session) {
            if let Some(found) = pick(&ev) {
                return found;
            }
        }
    }
}

async fn pending(
    holly: &entanglement_core::Holly,
    sub: &mut Receiver<OutEvent>,
) -> Vec<PendingApproval> {
    holly
        .send_from_wire(InMsg::ListPending {
            correlation_id: "envelope".into(),
            root: None,
        })
        .await
        .expect("ListPending is wire-allowed");
    next(sub, &sid(), |ev| match ev {
        OutEvent::PendingList {
            correlation_id,
            approvals,
        } if correlation_id == "envelope" => Some(approvals.clone()),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn a_model_call_to_an_envelope_tool_is_carded_under_its_target() {
    let input = serde_json::json!({ "tool": "get_stock_level", "args": { "sku": "A-17" } });
    let s = spawn(
        scripted(vec![call("c1", ENVELOPE, input)]),
        mode_table(&[], &[TARGET]),
        true,
        SYSTEM,
    );
    let mut sub = s.holly.subscribe();
    s.holly
        .send(InMsg::prompt(sid(), "stock of A-17?"))
        .await
        .expect("engine accepts the prompt");

    let (request_id, card) = next(&mut sub, &sid(), |ev| match ev {
        OutEvent::ToolRequest {
            request_id, tool, ..
        } => Some((request_id.clone(), tool.clone())),
        _ => None,
    })
    .await;
    assert_eq!(card, TARGET, "the card names the target, not {ENVELOPE}");
    // The id stays the model's own call id: the head answers that call.
    assert_eq!(request_id, "c1");

    let listed = pending(&s.holly, &mut sub).await;
    let [entry] = listed.as_slice() else {
        panic!("one parked card: {listed:#?}");
    };
    assert_eq!(entry.tool, TARGET);
    assert_eq!(entry.kind, ApprovalKind::Tool);
    assert_eq!(entry.reason, format!("mode `build` grades `{TARGET}` Ask"));

    s.holly
        .send(InMsg::Reject {
            session: sid(),
            request_id,
            reason: Some("not now".into()),
        })
        .await
        .expect("engine accepts the rejection");
    let (output, is_error) = next(&mut sub, &sid(), |ev| match ev {
        OutEvent::ToolOutput {
            output, is_error, ..
        } => Some((output.clone(), *is_error)),
        _ => None,
    })
    .await;
    assert!(is_error, "{output}");
    assert!(
        output.contains(&format!("tool `{TARGET}` rejected: not now")),
        "the refusal names what the model asked for: {output}"
    );
    assert!(s.calls.lock().unwrap().is_empty(), "the target never ran");
}
