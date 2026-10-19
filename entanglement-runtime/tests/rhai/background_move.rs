//! A compaction moves a background script's parked card to the successor
//! (#871, ADR-0238): the successor's root-filtered `ListPending` and its
//! handle-less `poll` list it, the card is re-announced there `detached`, a
//! `Stop` on the successor unwinds it, and an answer addressed to either id
//! resolves the script.

use std::time::Duration;

use entanglement_core::{Holly, InMsg, OutEvent, PendingApproval, SessionId};
use entanglement_runtime::script_ops::ScriptRegistry;
use tokio::sync::broadcast::Receiver;

use super::background_park::{approve_and_finish, engine, park_nested_edit};
use super::TempDir;

const WAIT: Duration = Duration::from_secs(5);

/// The card as a compaction re-announced it on the successor.
struct Moved {
    request_id: String,
    detached: bool,
}

/// Compact `pred` into `succ` (the exact `Spawn` + `CloseSession` pair the
/// fork sends, ADR-0205/0231) and return the card re-announced on `succ`.
async fn compact(holly: &Holly, pred: &SessionId, succ: &SessionId) -> Moved {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: succ.clone(),
            parent: None,
            predecessor: Some(pred.clone()),
            agent: "general".into(),
            prompt: "continue".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: pred.clone(),
        })
        .await
        .unwrap();
    // The executor moves cards on its own copy of the outbox, so the re-announced
    // card and the predecessor's end land in either order: wait for both.
    let (mut card, mut gone) = (None, false);
    while card.is_none() || !gone {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                session,
                request_id,
                tool,
                detached,
                ..
            })) if &session == succ && tool.contains("edit") => {
                card = Some(Moved {
                    request_id,
                    detached,
                })
            }
            Ok(Ok(OutEvent::SessionEnded { session, .. })) if &session == pred => gone = true,
            Ok(Ok(_)) => {}
            other => panic!("expected the card re-announced on {succ} and {pred} ended: {other:?}"),
        }
    }
    card.expect("the loop exits only with a card")
}

/// `ListPending` filtered to `root`'s spawn sub-tree.
async fn pending_under(holly: &Holly, root: &SessionId) -> Vec<PendingApproval> {
    let mut sub = holly.subscribe();
    let correlation = format!("root-{root}");
    holly
        .send_from_wire(InMsg::ListPending {
            correlation_id: correlation.clone(),
            root: Some(root.clone()),
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

/// `session`'s handle-less `poll` reply text.
async fn poll_text(holly: &Holly, session: &SessionId) -> String {
    let mut sub: Receiver<OutEvent> = holly.subscribe();
    holly
        .send(InMsg::prompt(session.clone(), "poll"))
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::ToolOutput {
                session: s,
                tool,
                output,
                ..
            })) if &s == session && tool == "poll" => return output,
            Ok(Ok(_)) => {}
            other => panic!("expected {session}'s poll output: {other:?}"),
        }
    }
}

async fn compacted_card(name: &str) -> (TempDir, ScriptRegistry, Holly, SessionId, SessionId) {
    let dir = TempDir::new(name);
    let scripts = ScriptRegistry::new();
    let holly = engine(&dir.path, scripts.clone());
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    let parked = park_nested_edit(&holly, &pred).await;
    let card = compact(&holly, &pred, &succ).await;
    assert_eq!(card.request_id, parked, "the same park, re-announced");
    assert!(card.detached, "the re-announced card is detached");
    (dir, scripts, holly, pred, succ)
}

#[tokio::test]
async fn the_successor_lists_the_moved_card_and_approves_it() {
    let (dir, scripts, holly, pred, succ) = compacted_card("bg-move-succ").await;

    let listed = pending_under(&holly, &succ).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].session, succ, "reported on the successor");
    assert!(listed[0].detached);
    assert!(pending_under(&holly, &pred).await.is_empty());

    let polled = poll_text(&holly, &succ).await;
    let row = format!("{} — rhai approval for", listed[0].request_id);
    assert!(polled.contains("parked approvals:"), "{polled}");
    assert!(polled.contains(&row), "{polled}");
    assert!(polled.contains("session succ"), "{polled}");

    let request_id = listed[0].request_id.clone();
    approve_and_finish(&holly, &scripts, &succ, &succ, request_id, &dir.path).await;
    assert!(pending_under(&holly, &succ).await.is_empty());
}

/// A head that showed the card on the predecessor's view (or an embedder
/// that cached its id) answers with the old key: it must not be a dead card.
#[tokio::test]
async fn approving_under_the_predecessors_key_still_resolves() {
    let (dir, scripts, holly, pred, succ) = compacted_card("bg-move-pred").await;
    let request_id = pending_under(&holly, &succ).await[0].request_id.clone();
    approve_and_finish(&holly, &scripts, &pred, &succ, request_id, &dir.path).await;
    assert!(pending_under(&holly, &succ).await.is_empty());
}

/// `Stop` on the successor is the escape hatch for the moved card too
/// (ADR-0237 §4): the script's nested call fails and the run ends.
#[tokio::test]
async fn a_stop_on_the_successor_unwinds_the_moved_card() {
    let (dir, scripts, holly, _pred, succ) = compacted_card("bg-move-stop").await;
    let handle = scripts
        .snapshot_ops(Some(&succ))
        .into_iter()
        .next()
        .expect("the successor owns the script (ADR-0231)")
        .handle;

    holly
        .send(InMsg::Stop {
            session: succ.clone(),
        })
        .await
        .unwrap();
    let mut output = Vec::new();
    let mut finished = false;
    for _ in 0..20 {
        let polled = scripts
            .poll(&handle, &succ, false, 1)
            .await
            .expect("the owner can poll the script");
        output.extend(polled.output);
        if !polled.running {
            finished = true;
            break;
        }
    }
    let output = String::from_utf8_lossy(&output);
    assert!(finished, "the script must end after the Stop: {output}");
    assert!(output.contains("rhai run stopped"), "{output}");
    assert!(pending_under(&holly, &succ).await.is_empty());
    assert!(!dir.path.join("f.txt").exists(), "the edit never ran");
}

/// Every call here has id `t1`, so a script the successor launches parks a
/// nested card with the moved card's id. The moved card is never displaced:
/// the successor's park is refused with a reason its script sees, and the
/// moved card is still listed on the successor and answers there.
#[tokio::test]
async fn the_successors_own_park_of_a_moved_cards_id_is_refused() {
    let (dir, scripts, holly, _pred, succ) = compacted_card("bg-move-clash").await;
    let moved = pending_under(&holly, &succ).await[0].request_id.clone();

    holly.send(InMsg::prompt(succ.clone(), "go")).await.unwrap();
    let refusal = tokio::time::timeout(WAIT, async {
        let mut seen: std::collections::HashMap<String, Vec<u8>> = Default::default();
        loop {
            // Only a finished script: polling the parked one would wait.
            for op in scripts
                .snapshot_ops(Some(&succ))
                .into_iter()
                .filter(|op| !op.running)
            {
                let polled = scripts
                    .poll(&op.handle, &succ, false, 0)
                    .await
                    .expect("the successor owns both scripts");
                let out = seen.entry(op.handle.clone()).or_default();
                out.extend(polled.output);
                let text = String::from_utf8_lossy(out).into_owned();
                if !polled.running && text.contains("already parked") {
                    return text;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the successor's script ends on the refusal");
    assert!(refusal.contains(&moved), "{refusal}");

    let listed = pending_under(&holly, &succ).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_id, moved);
    holly
        .send(InMsg::Approve {
            session: succ.clone(),
            request_id: moved,
            scope: Default::default(),
            mode: None,
            run_all: false,
        })
        .await
        .unwrap();
    let f = dir.path.join("f.txt");
    tokio::time::timeout(WAIT, async {
        while std::fs::read_to_string(&f).ok().as_deref() != Some("approved\n") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the moved card's approved edit ran");
    assert!(pending_under(&holly, &succ).await.is_empty());
}
