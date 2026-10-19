use super::super::test_support::detached_card;
use super::super::{approval, await_decision};

/// `await_decision`, bounded: a waiter a mutant strands fails, not hangs.
async fn decided(rx: oneshot::Receiver<Decision>) -> Decision {
    tokio::time::timeout(std::time::Duration::from_secs(5), await_decision(rx))
        .await
        .expect("the waiter was decided")
}
use super::*;
use entanglement_core::ApprovalKind;

/// Park `card` and return its receiver plus the sessions its event named.
fn park(
    pending: &PendingDecisions,
    card: WireApproval,
) -> (oneshot::Receiver<Decision>, Vec<String>) {
    let mut emitted = Vec::new();
    let rx = pending.register_parked(card, "edit", |a| emitted.push(a.session.0.clone()));
    (rx, emitted)
}

/// Move `p`'s cards to `s`, returning the request ids announced.
fn move_cards(pending: &PendingDecisions, p: &SessionId, s: &SessionId) -> Vec<String> {
    let mut announced = Vec::new();
    pending.reassign_detached(p, s, |a| announced.push(a.request_id.clone()));
    announced
}

fn reject() -> Decision {
    Decision::Reject { reason: None }
}

async fn rejected(rx: oneshot::Receiver<Decision>) -> bool {
    matches!(decided(rx).await, Decision::Reject { .. })
}

/// The refusal a park sharing a live detached card's id gets.
async fn refused(rx: oneshot::Receiver<Decision>) -> bool {
    matches!(
        decided(rx).await,
        Decision::Reject { reason: Some(r) } if r.contains("already parked")
    )
}

fn listed(pending: &PendingDecisions) -> Vec<(String, String)> {
    pending
        .snapshot(None)
        .into_iter()
        .map(|a| (a.session.0, a.request_id))
        .collect()
}

#[tokio::test]
async fn a_detached_card_moves_and_answers_under_either_key() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let via_s = pending.register_approval(detached_card(&p, "d1"), "edit");
    let via_p = pending.register_approval(detached_card(&p, "d2"), "edit");
    let turn = pending.register(&p, "t", "tool", "bash");

    let mut moved = move_cards(&pending, &p, &s);
    moved.sort();
    assert_eq!(moved, ["d1", "d2"]);
    assert!(move_cards(&pending, &p, &s).is_empty(), "idempotent");
    let sessions: Vec<_> = listed(&pending).into_iter().map(|(s, _)| s).collect();
    assert_eq!(sessions, ["p", "s", "s"], "the turn card stays");
    let approvals = pending.approvals(|_| true);
    assert!(approvals
        .iter()
        .filter(|a| a.detached)
        .all(|a| a.session == s));

    pending.resolve(&s, "d1", reject());
    pending.resolve(&p, "d2", reject());
    assert!(rejected(via_s).await);
    assert!(rejected(via_p).await);
    pending.stop_attached(&p);
    assert!(matches!(decided(turn).await, Decision::Stop));
}

#[tokio::test]
async fn a_later_detached_park_is_shown_on_the_successor_and_stop_unwinds_it() {
    let pending = PendingDecisions::default();
    let (p, s, t) = (
        SessionId::new("p"),
        SessionId::new("s"),
        SessionId::new("t"),
    );
    move_cards(&pending, &p, &s);
    move_cards(&pending, &s, &t);
    let (rx, emitted) = park(&pending, detached_card(&p, "d"));
    assert_eq!(emitted, ["t"], "emitted on the end of the chain");
    // An attended card of the same session is never redirected.
    let (_turn, emitted) = park(
        &pending,
        approval(&p, "a", ApprovalKind::Tool, "bash", "ls", "why", None),
    );
    assert_eq!(emitted, ["p"]);
    pending.stop_session(&p);
    pending.stop_session(&s);
    assert!(listed(&pending).contains(&("t".into(), "d".into())));
    pending.stop_session(&t);
    assert!(matches!(decided(rx).await, Decision::Stop));
}

/// Gemini's `name#ordinal` ids repeat, so the successor can park its own
/// card with a moved card's id. The moved card is never displaced (it would
/// be stranded on an ended session): the successor's park is refused with
/// a reason the model sees, and the moved card still answers on `s`.
#[tokio::test]
async fn the_successors_own_park_of_a_moved_cards_id_is_refused() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let rid = "rhai#0:rhai:edit";
    let moved = pending.register_approval(detached_card(&p, rid), "edit");
    move_cards(&pending, &p, &s);

    let (own, emitted) = park(&pending, detached_card(&s, rid));
    assert!(emitted.is_empty(), "a refused park emits no card");
    assert!(refused(own).await);
    // A foreground repeat of the id is refused the same way.
    let (turn, _) = park(
        &pending,
        approval(&s, rid, ApprovalKind::Tool, "edit", "{}", "why", None),
    );
    assert!(refused(turn).await);

    assert_eq!(listed(&pending), [("s".into(), rid.into())]);
    pending.resolve(&s, rid, reject());
    assert!(rejected(moved).await);
}

/// A fresh detached park whose lineage ends at a session already showing
/// its id is refused too, rather than kept on the ended predecessor.
#[tokio::test]
async fn a_fresh_detached_park_into_an_id_the_successor_shows_is_refused() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let shown = pending.register(&s, "r", "tool", "bash");
    move_cards(&pending, &p, &s);
    let (fresh, emitted) = park(&pending, detached_card(&p, "r"));
    assert!(emitted.is_empty());
    assert!(refused(fresh).await);
    assert_eq!(listed(&pending), [("s".into(), "r".into())]);
    pending.resolve(&s, "r", reject());
    assert!(rejected(shown).await);
}

/// Once the moved card is answered on the successor, a stale decision or
/// timeout under the predecessor's id must not reach the successor's own
/// card with the same id.
#[tokio::test]
async fn a_stale_predecessor_id_never_reaches_the_successors_own_card() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let moved = pending.register_approval(detached_card(&p, "r"), "edit");
    move_cards(&pending, &p, &s);
    pending.resolve(&s, "r", reject());
    assert!(
        rejected(moved).await,
        "the successor's id answers the moved card"
    );

    let own = pending.register_approval(detached_card(&s, "r"), "edit");
    pending.resolve(&p, "r", Decision::Stop);
    pending.forget(&p, "r");
    assert_eq!(listed(&pending), [("s".into(), "r".into())]);
    pending.resolve(&s, "r", reject());
    assert!(rejected(own).await);
}

/// The narrow-race fallback: a move that would put a second live card with
/// one id on the target leaves the card where it is, unannounced, and each
/// card keeps answering under its own key. A later move, once the id is
/// free, carries it over.
#[tokio::test]
async fn a_move_that_would_collide_stays_put_and_is_not_announced() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let own = pending.register(&s, "r", "tool", "bash");
    let stayed = pending.register_approval(detached_card(&p, "r"), "edit");
    assert!(move_cards(&pending, &p, &s).is_empty());
    assert_eq!(
        listed(&pending),
        [("p".into(), "r".into()), ("s".into(), "r".into())]
    );
    pending.resolve(&s, "r", reject());
    assert!(
        rejected(own).await,
        "the successor's id answers its own card"
    );
    assert_eq!(move_cards(&pending, &p, &s), ["r"], "the id is free now");
    pending.resolve(&p, "r", reject());
    assert!(rejected(stayed).await);
}

/// A move from a session that compacted twice lands on the live end.
#[test]
fn a_move_lands_on_the_end_of_the_lineage() {
    let pending = PendingDecisions::default();
    let (p, s, t) = (
        SessionId::new("p"),
        SessionId::new("s"),
        SessionId::new("t"),
    );
    let _card = pending.register_approval(detached_card(&p, "r"), "edit");
    move_cards(&pending, &s, &t);
    move_cards(&pending, &p, &s);
    assert_eq!(listed(&pending), [("t".into(), "r".into())]);
}

/// Only a turn card supersedes a live turn card: a detached park under a
/// live turn card's key is refused, whichever side parks first.
#[tokio::test]
async fn a_detached_park_never_supersedes_a_live_turn_card() {
    let pending = PendingDecisions::default();
    let s = SessionId::new("s");
    let turn = pending.register(&s, "t", "tool", "bash");
    let (script, emitted) = park(&pending, detached_card(&s, "t"));
    assert!(emitted.is_empty());
    assert!(refused(script).await);
    pending.resolve(&s, "t", reject());
    assert!(rejected(turn).await, "the turn card was not displaced");
}

/// `forget` (a timeout's cleanup) takes only its own key, and only once
/// its waiter is gone: a live card is never forgotten, nor one moved onto
/// that id from elsewhere.
#[tokio::test]
async fn forget_removes_only_its_own_closed_entry() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let live = pending.register_approval(detached_card(&p, "a"), "edit");
    move_cards(&pending, &p, &s);
    pending.forget(&s, "a");
    pending.forget(&p, "a");
    assert_eq!(listed(&pending), [("s".into(), "a".into())], "live: kept");
    drop(pending.register(&s, "gone", "tool", "bash"));
    pending.forget(&s, "gone");
    pending.resolve(&s, "a", reject());
    assert!(rejected(live).await);
    assert!(pending.lock().waiters.is_empty());
}

/// The reviewer's probe: `(s, r)` timed out (closed, not yet forgotten)
/// when `(p, r)` moves onto `s`. The dead entry does not block the move
/// (only live ids count), the late `forget` removes just that dead entry,
/// and live-first `resolve` reaches the moved card.
#[tokio::test]
async fn a_late_timeout_forget_never_takes_a_card_moved_onto_its_id() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    drop(pending.register(&s, "r", "tool", "bash"));
    let moved = pending.register_approval(detached_card(&p, "r"), "edit");
    assert_eq!(move_cards(&pending, &p, &s), ["r"], "announced");
    pending.forget(&s, "r");
    assert_eq!(listed(&pending), [("s".into(), "r".into())]);
    pending.resolve(&s, "r", reject());
    assert!(rejected(moved).await);
    assert!(pending.lock().waiters.is_empty(), "no leaked entry");
}

/// A turn card's second park supersedes it, stopping the old waiter (a
/// resume's re-offer after a lagged-away `SessionHibernated`); a background
/// script's live card is never displaced.
#[tokio::test]
async fn a_live_turn_card_is_superseded_but_a_detached_one_is_not() {
    let pending = PendingDecisions::default();
    let s = SessionId::new("s");
    let first = pending.register(&s, "t", "tool", "bash");
    let second = pending.register(&s, "t", "tool", "bash");
    assert!(matches!(decided(first).await, Decision::Stop));
    pending.resolve(&s, "t", reject());
    assert!(rejected(second).await);

    let card = pending.register_approval(detached_card(&s, "d"), "edit");
    let (again, emitted) = park(&pending, detached_card(&s, "d"));
    assert!(emitted.is_empty());
    assert!(refused(again).await);
    assert!(refused(pending.register(&s, "d", "tool", "bash")).await);
    pending.resolve(&s, "d", reject());
    assert!(rejected(card).await);
}

/// A waiter that gave up never shadows a live one with the same id.
#[tokio::test]
async fn a_decision_prefers_a_live_moved_card_over_one_that_gave_up() {
    let pending = PendingDecisions::default();
    let (p, q, s) = (
        SessionId::new("p"),
        SessionId::new("q"),
        SessionId::new("s"),
    );
    drop(pending.register_approval(detached_card(&p, "r"), "edit"));
    let live = pending.register_approval(detached_card(&q, "r"), "edit");
    move_cards(&pending, &p, &s);
    move_cards(&pending, &q, &s);
    pending.resolve(&s, "r", reject());
    assert!(rejected(live).await);
}

#[test]
fn a_lineage_cycle_does_not_spin() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    move_cards(&pending, &p, &s);
    move_cards(&pending, &s, &p);
    assert!(move_cards(&pending, &p, &p).is_empty());
    let (_rx, emitted) = park(&pending, detached_card(&p, "r"));
    assert_eq!(emitted, ["s"]);
}
