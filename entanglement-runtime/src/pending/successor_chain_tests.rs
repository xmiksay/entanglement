//! #888 (ADR-0242): the heals walk the whole compaction chain, over an
//! engine whose directory records the chain while no executor fold ran —
//! what lagged `SessionStarted`s leave.

use super::super::test_support::{detached_card, spawn_session};
use super::super::{approval, park_approval_as};
use super::tests::{decided, listed, park_on, refused, reject, rejected};
use super::*;
use entanglement_core::ApprovalKind;

/// An engine whose directory records `ids` as one compaction chain, oldest
/// first, every session live and no executor fold run — what lagged
/// `SessionStarted`s leave.
async fn chain(ids: &[&SessionId]) -> Holly {
    let holly = Holly::spawn(entanglement_core::EngineConfig::default());
    let mut prev = None;
    for id in ids {
        spawn_session(&holly, id, None, prev).await;
        prev = Some(*id);
    }
    holly
}

/// Close `s` and wait until it is no longer live.
async fn end(holly: &Holly, s: &SessionId) {
    holly
        .send(InMsg::CloseSession { session: s.clone() })
        .await
        .expect("engine accepts CloseSession");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while holly.is_live(s) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the session ends");
}

#[tokio::test]
async fn a_later_detached_park_is_shown_on_the_successor_and_stop_unwinds_it() {
    let pending = PendingDecisions::default();
    let (p, s, t) = (
        SessionId::new("p"),
        SessionId::new("s"),
        SessionId::new("t"),
    );
    let holly = chain(&[&p, &s, &t]).await;
    let (rx, emitted) = park_on(&pending, detached_card(&p, "d"), Some(&holly));
    assert_eq!(emitted, ["t"], "emitted on the end of the chain");
    // An attended card of the same session is never redirected.
    let (_turn, emitted) = park_on(
        &pending,
        approval(&p, "a", ApprovalKind::Tool, "bash", "ls", "why", None),
        Some(&holly),
    );
    assert_eq!(emitted, ["p"]);
    pending.stop_session(&p);
    pending.stop_session(&s);
    assert!(listed(&pending).contains(&("t".into(), "d".into())));
    pending.stop_session(&t);
    assert!(matches!(decided(rx).await, Decision::Stop));
}

/// A fresh detached park whose lineage ends at a session already showing
/// its id is refused too, rather than kept on the ended predecessor.
#[tokio::test]
async fn a_fresh_detached_park_into_an_id_the_successor_shows_is_refused() {
    let pending = PendingDecisions::default();
    let (p, s) = (SessionId::new("p"), SessionId::new("s"));
    let holly = chain(&[&p, &s]).await;
    let shown = pending.register(&s, "r", "tool", "bash");
    let (fresh, emitted) = park_on(&pending, detached_card(&p, "r"), Some(&holly));
    assert!(emitted.is_empty());
    assert!(refused(fresh).await);
    assert_eq!(listed(&pending), [("s".into(), "r".into())]);
    pending.resolve(&s, "r", reject());
    assert!(rejected(shown).await);
}

/// #888: two compactions (p0 → p → s) before any fold or heal ran. The
/// heal walks the whole chain back, so the oldest ancestor's card lands on
/// `s` and is announced there; one hop would have moved only `p`'s.
#[tokio::test]
async fn a_heal_moves_a_card_two_compactions_back() {
    let (p0, p, s) = (
        SessionId::new("p0"),
        SessionId::new("p"),
        SessionId::new("s"),
    );
    let holly = chain(&[&p0, &p, &s]).await;
    end(&holly, &p0).await;
    end(&holly, &p).await;
    let pending = PendingDecisions::announcing(holly.clone());
    let card = pending.register_approval(detached_card(&p0, "r"), "edit");
    let mut sub = holly.subscribe();
    heal_lineage(&holly, &pending, &s, |_, _| true);
    assert_eq!(listed(&pending), [("s".into(), "r".into())]);
    match sub.try_recv() {
        Ok(entanglement_core::OutEvent::ToolRequest {
            session, detached, ..
        }) => assert!(session == s && detached, "announced on {session}"),
        other => panic!("expected the moved card announced: {other:?}"),
    }
    pending.resolve(&s, "r", reject());
    assert!(rejected(card).await);
}

/// The forward case p → s → s2 with the middle link `s` ended: a park keyed
/// on `p` lands on `s2` (`successor_of` already skips the retired middle),
/// and the backward walk from `s2` takes `p`'s earlier card with it.
#[tokio::test]
async fn a_park_past_an_ended_middle_link_lands_on_the_live_end() {
    let (p, s, s2) = (
        SessionId::new("p"),
        SessionId::new("s"),
        SessionId::new("s2"),
    );
    let holly = chain(&[&p, &s, &s2]).await;
    end(&holly, &p).await;
    end(&holly, &s).await;
    let pending = PendingDecisions::default();
    let early = pending.register_approval(detached_card(&p, "early"), "edit");
    let late = park_approval_as(&holly, &pending, detached_card(&p, "late"), "edit", true);
    let mut sessions: Vec<_> = listed(&pending).into_iter().map(|(s, _)| s).collect();
    sessions.dedup();
    assert_eq!(sessions, ["s2"]);
    pending.resolve(&p, "early", reject());
    pending.resolve(&s2, "late", reject());
    assert!(rejected(early).await);
    assert!(rejected(late).await);
}

/// A decision under the id of a link a card moved *through* answers it: a
/// head still showing the middle link's copy (it missed the successor's
/// `SessionStarted`) is not left with a dead card. Before the card reached
/// that link, the same decision finds nothing.
#[tokio::test]
async fn a_middle_links_id_answers_a_card_moved_through_it() {
    let (p0, p, s) = (
        SessionId::new("p0"),
        SessionId::new("p"),
        SessionId::new("s"),
    );
    let holly = chain(&[&p0, &p, &s]).await;
    let pending = PendingDecisions::default();
    let card = pending.register_approval(detached_card(&p0, "r"), "edit");
    pending.resolve(&p, "r", reject());
    assert_eq!(
        listed(&pending),
        [("p0".into(), "r".into())],
        "not moved yet"
    );
    pending.reassign_detached(std::slice::from_ref(&p0), &p, |_, _| {});
    heal_lineage(&holly, &pending, &s, |_, _| true);
    assert_eq!(listed(&pending), [("s".into(), "r".into())]);
    pending.resolve(&p, "r", reject());
    assert!(rejected(card).await);
    // A card the successor parked itself is never reached from upstream.
    let own = pending.register_approval(detached_card(&s, "o"), "edit");
    pending.resolve(&p, "o", reject());
    assert_eq!(listed(&pending), [("s".into(), "o".into())]);
    drop(own);
}

/// Review regression: a card parked straight onto the live end (p → s →
/// s2) was never shown on `s`, so a stale decision for `s`'s own,
/// already-answered card with the same id (Gemini ids repeat) must not
/// reach it.
#[tokio::test]
async fn a_stale_middle_decision_never_reaches_a_card_parked_past_it() {
    let (p, s, s2) = (
        SessionId::new("p"),
        SessionId::new("s"),
        SessionId::new("s2"),
    );
    let holly = chain(&[&p, &s, &s2]).await;
    let pending = PendingDecisions::default();
    let own = pending.register_approval(detached_card(&s, "r"), "edit");
    pending.resolve(&s, "r", reject());
    assert!(rejected(own).await);
    let _card = park_approval_as(&holly, &pending, detached_card(&p, "r"), "edit", true);
    assert_eq!(listed(&pending), [("s2".into(), "r".into())]);
    pending.resolve(&s, "r", reject());
    assert_eq!(
        listed(&pending),
        [("s2".into(), "r".into())],
        "still waiting"
    );
}
