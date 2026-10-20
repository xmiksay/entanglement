//! A background script's card across a chain of compactions (#888,
//! ADR-0242): every move walks the whole compaction chain, so a card parked
//! two compactions back still reaches the live end, and a decision under a
//! middle link's id still answers a card that moved through it.

use entanglement_core::{ApprovalScope, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::{error::TryRecvError, Receiver};

use super::background_follow::{any, approve, compact, edit_card_on, executor_caught_up, until};
use super::background_follow_mode::{engine, gate, wait_for_file, wait_on};
use super::TempDir;

fn ids3(a: &str, b: &str, c: &str) -> (SessionId, SessionId, SessionId) {
    (SessionId::new(a), SessionId::new(b), SessionId::new(c))
}

/// `session`'s events still buffered in `log`, as resume records.
fn records_of(log: &mut Receiver<OutEvent>, session: &SessionId) -> Vec<(Option<InMsg>, OutEvent)> {
    let mut records = Vec::new();
    loop {
        match log.try_recv() {
            Ok(ev) if ev.session() == Some(session) => records.push((None, ev)),
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(_) => return records,
        }
    }
}

fn request_id(card: OutEvent) -> String {
    match card {
        OutEvent::ToolRequest { request_id, .. } => request_id,
        other => panic!("not a card: {other:?}"),
    }
}

/// Two compactions (p0 → p → s) while the script waits, then `s`
/// hibernates, so the script's card parks on the retired `p0` — nothing
/// live to move it to. When `s` resumes, its `SessionStarted` fold walks the
/// chain back past `p` and moves the card to `s`; one hop moved only `p`'s
/// cards, leaving it on `p0`.
#[tokio::test]
async fn a_card_parked_two_compactions_back_moves_to_the_resumed_successor() {
    let dir = TempDir::new("bg-chain-two-back");
    let (g1, started) = (gate(&dir, "g1"), gate(&dir, "started"));
    let script = format!(
        r#"{} edit("f.txt", "", "approved\n"); "finished""#,
        wait_on(&g1, Some(&started))
    );
    let (holly, _modes) = engine(&dir.path, script, None);
    let (p0, p, s) = ids3("p0", "p", "s");
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(p0.clone(), "go")).await.unwrap();
    wait_for_file(&started).await;
    compact(&holly, &mut sub, &p0, &p).await;
    executor_caught_up(&holly, &mut sub, &p).await;
    let mut log = holly.subscribe();
    compact(&holly, &mut sub, &p, &s).await;
    executor_caught_up(&holly, &mut sub, &s).await;
    holly
        .send(InMsg::HibernateSession { session: s.clone() })
        .await
        .unwrap();
    until(
        &mut sub,
        "the successor hibernated",
        any,
        |ev| matches!(ev, OutEvent::SessionHibernated { session, .. } if session == &s),
    )
    .await;
    let records = records_of(&mut log, &s);

    std::fs::write(&g1, "").unwrap();
    until(&mut sub, "the card parked on p0", any, |ev| {
        edit_card_on(ev, &p0)
    })
    .await;
    holly.resume(s.clone(), records).await.unwrap();
    let card = until(&mut sub, "the card moved to s", any, |ev| {
        edit_card_on(ev, &s)
    })
    .await;
    approve(&holly, &s, &request_id(card), ApprovalScope::Once).await;
    wait_for_file(&dir.path.join("f.txt")).await;
}

/// The forward case p → s → s2, the middle link `s` ended: the card parked
/// on `p` moves to `s`, then on to `s2`. A head that still shows `s`'s copy
/// (it missed `s2`'s `SessionStarted`) answers under `s`'s id, and that
/// reaches the card: it moved through `s`. Before, only `p`'s id and
/// `s2`'s did, and the answer was silently dropped.
#[tokio::test]
async fn a_card_moved_twice_answers_under_the_middle_links_id() {
    let dir = TempDir::new("bg-chain-middle");
    let script = r#"edit("f.txt", "", "approved\n"); "finished""#.to_string();
    let (holly, _modes) = engine(&dir.path, script, None);
    let (p, s, s2) = ids3("p", "s", "s2");
    let mut sub = holly.subscribe();
    let mut cards = holly.subscribe();
    holly.send(InMsg::prompt(p.clone(), "go")).await.unwrap();
    until(&mut cards, "the card on p", any, |ev| edit_card_on(ev, &p)).await;
    compact(&holly, &mut sub, &p, &s).await;
    compact(&holly, &mut sub, &s, &s2).await;
    let card = until(&mut cards, "the card on s2", any, |ev| {
        edit_card_on(ev, &s2)
    })
    .await;
    let request_id = request_id(card);
    assert!(!holly.is_live(&s), "the middle link ended");

    approve(&holly, &s, &request_id, ApprovalScope::Once).await;
    until(&mut cards, "the card's end on s2", any, |ev| {
        matches!(ev, OutEvent::ApprovalResolved { session, request_id: r, .. }
            if session == &s2 && r == &request_id)
    })
    .await;
    wait_for_file(&dir.path.join("f.txt")).await;
}
