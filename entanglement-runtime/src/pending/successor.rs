//! Moving a background script's parked cards to a compaction successor
//! (#871, ADR-0238).
//!
//! WHY: a background `rhai` script outlives its session's compaction and
//! follows the successor (ADR-0231), but its nested card belonged to the
//! predecessor, so the successor's root-filtered `ListPending`, its
//! handle-less `poll` and its `Stop` never reached it. A card's map key
//! never changes — the predecessor's id keeps answering it — only its `home`
//! (where it is shown, listed and stopped) moves. The move runs at the
//! ADR-0231 points: the lifecycle fold on the successor's `SessionStarted`,
//! and a self-heal from the supervisor's directory before anything reads,
//! answers or stops the successor's cards.

use entanglement_core::{Holly, InMsg, PendingApproval as WireApproval, SessionId};
use tokio::sync::oneshot;

use super::{Inner, Parked, PendingDecisions};
use crate::agent_registry::AgentRegistry;
use crate::seam::Decision;

type Key = (SessionId, String);

impl Inner {
    /// Where a card parked by `session`'s detached work is shown now: the
    /// end of its recorded compaction lineage. Bounded by the map's size, so
    /// a cycle can't spin the walk.
    pub(super) fn current(&self, session: &SessionId) -> SessionId {
        let mut at = session.clone();
        for _ in 0..self.successors.len() {
            match self.successors.get(&at) {
                Some(next) if next != session => at = next.clone(),
                _ => break,
            }
        }
        at
    }

    /// Insert under `key`, home `parked.home`. Only a turn card supersedes a
    /// live turn card under the same key, stopping it (a resume's re-offer
    /// after a lagged-away `SessionHibernated`), as before ADR-0238. Any other
    /// park sharing a live card's key, or its id on its home, is refused:
    /// `Err` carries an already-rejected receiver, so the call fails with
    /// the reason, and a live detached card is never displaced.
    pub(super) fn insert(
        &mut self,
        session: &SessionId,
        request_id: &str,
        parked: Parked,
    ) -> Result<oneshot::Receiver<Decision>, oneshot::Receiver<Decision>> {
        let key = (session.clone(), request_id.to_string());
        let (tx, rx) = oneshot::channel();
        let blocked = self.waiters.iter().any(|(k, (t, p))| {
            let clashes = k == &key || (k.1 == request_id && p.home == parked.home);
            clashes && !t.is_closed() && (p.detached || parked.detached || k != &key)
        });
        if blocked {
            tracing::warn!(%session, request_id, "a live card already holds this id; park refused");
            let reason = format!("an approval for request `{request_id}` is already parked");
            let _ = tx.send(Decision::Reject {
                reason: Some(reason),
            });
            return Err(rx);
        }
        if let Some((superseded, _)) = self.waiters.insert(key, (tx, parked)) {
            let _ = superseded.send(Decision::Stop);
        }
        Ok(rx)
    }

    /// The key a decision for `(session, request_id)` answers: its exact key
    /// or a card moved onto `session` with that id. A home never holds two
    /// live cards with one id (the park guard and the move's skip), so at
    /// most one candidate is live; it wins over any that gave up. Only a
    /// card that moved there matches, so a predecessor's id never reaches
    /// the successor's own card.
    fn find(&self, session: &SessionId, request_id: &str) -> Option<Key> {
        let mut candidates: Vec<(&Key, bool)> = self
            .waiters
            .iter()
            .filter(|(k, (_, p))| {
                k.1 == request_id && (&k.0 == session || (&p.home == session && &k.0 != session))
            })
            .map(|(k, (tx, _))| (k, tx.is_closed()))
            .collect();
        candidates.sort_by_key(|(k, closed)| (*closed, k.0 .0.clone()));
        candidates.first().map(|(k, _)| (*k).clone())
    }

    /// A timed-out park's own entry, by its exact key and only once its
    /// waiter is gone: a live card moved onto that id is someone else's.
    pub(super) fn forget(&mut self, session: &SessionId, request_id: &str) {
        let key = (session.clone(), request_id.to_string());
        if self.waiters.get(&key).is_some_and(|(tx, _)| tx.is_closed()) {
            self.waiters.remove(&key);
        }
    }

    pub(super) fn remove(
        &mut self,
        session: &SessionId,
        request_id: &str,
    ) -> Option<(oneshot::Sender<Decision>, Parked)> {
        let key = self.find(session, request_id)?;
        self.waiters.remove(&key)
    }

    /// Every live waiter with its home, the session a listing reports it
    /// under. A closed sender means the waiter is gone — a timeout removes
    /// its own entry (`await_timed`), but nothing removes an aborted task's
    /// until `Stop`.
    pub(super) fn listed(&self) -> Vec<(SessionId, &str, &Parked)> {
        self.waiters
            .iter()
            .filter(|(_, (tx, _))| !tx.is_closed())
            .map(|((_, rid), (_, parked))| (parked.home.clone(), rid.as_str(), parked))
            .collect()
    }
}

impl PendingDecisions {
    /// Move every detached card shown on `predecessor` to the live end of
    /// its lineage (`successor`, or past it if that compacted too), record
    /// the lineage, and call `emit` with each still-waiting moved approval —
    /// under the lock, so no decision can resolve a card between its move
    /// and its announcement. Idempotent. Turn cards stay: their turn ends
    /// with the predecessor (`stop_attached`). A card whose id the target
    /// already shows stays where it is, unannounced: a home never holds two
    /// live cards with one id. Parks heal first, so that is a narrow race.
    pub(crate) fn reassign_detached(
        &self,
        predecessor: &SessionId,
        successor: &SessionId,
        mut emit: impl FnMut(&WireApproval),
    ) {
        if predecessor == successor {
            return;
        }
        let mut guard = self.lock();
        guard
            .successors
            .insert(predecessor.clone(), successor.clone());
        let target = guard.current(predecessor);
        let shown: std::collections::HashSet<String> = guard
            .waiters
            .iter()
            .filter(|(_, (tx, p))| p.home == target && !tx.is_closed())
            .map(|((_, rid), _)| rid.clone())
            .collect();
        for ((_, rid), (tx, parked)) in guard.waiters.iter_mut() {
            if &parked.home != predecessor || !parked.detached || shown.contains(rid) {
                continue;
            }
            parked.home = target.clone();
            if let Some(approval) = parked.approval.as_mut() {
                approval.session = target.clone();
                if !tx.is_closed() {
                    emit(approval);
                }
            }
        }
    }
}

/// Announce a moved card on its new home: a fresh `ToolRequest`, same
/// request id and content, `detached: true`, so a head moves it there.
pub(crate) fn announce(holly: &Holly, approval: &WireApproval) {
    tracing::debug!(
        session = %approval.session,
        request_id = %approval.request_id,
        "moved a background script's card to the compaction successor"
    );
    super::park::emit_request(holly, approval);
}

/// The lossless self-heal: re-run the move from `session`'s predecessor as
/// the supervisor's directory records it, announcing the moved cards
/// `announce_if` accepts. A no-op for a session that is not a compaction
/// successor, or whose move already ran.
pub(crate) fn heal_from_predecessor(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    announce_if: impl Fn(&WireApproval) -> bool,
) {
    // The start-up window: the directory already names the predecessor, but
    // the seq counter does not exist yet. A move now would announce with
    // seq 0 (dropped by heads) and leave the `SessionStarted` fold nothing
    // to announce, losing the card; the fold moves it once the session is
    // live. (An ended session is already gone from the directory.)
    if !holly.is_live(session) {
        return;
    }
    if let Some(predecessor) = holly.session_info(session).and_then(|i| i.predecessor) {
        pending.reassign_detached(&predecessor, session, |a| {
            if announce_if(a) {
                announce(holly, a);
            }
        });
    }
}

/// The heal a park runs before registering (ADR-0238 §2): first the cards
/// `session`'s predecessor left (so the successor's own park meets a moved
/// card already on it and is refused, never the reverse), then — for a
/// detached park — `session`'s own cards forward along the supervisor's
/// successor chain, which is written before the successor's
/// `SessionStarted`, so the park lands on the live end.
pub(super) fn heal_for_park(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    detached: bool,
) {
    heal_from_predecessor(holly, pending, session, |_| true);
    if !detached {
        return;
    }
    let mut at = session.clone();
    let mut seen = vec![at.clone()];
    // Stop at a successor that is not live yet: its seq counter is missing,
    // and its `SessionStarted` fold carries the card over with a real seq.
    while let Some(next) = holly
        .successor_of(&at)
        .filter(|n| !seen.contains(n) && holly.is_live(n))
    {
        pending.reassign_detached(&at, &next, |a| announce(holly, a));
        seen.push(next.clone());
        at = next;
    }
}

/// Heal every session of `root`'s spawn sub-tree — `root` included — so a
/// compacted sub-agent's card is listed under its successor too. Run after
/// `AgentRegistry::heal_from`, so the walk reaches compacted children.
pub(crate) fn heal_subtree(
    holly: &Holly,
    pending: &PendingDecisions,
    agents: &AgentRegistry,
    root: &SessionId,
) {
    for session in crate::discover::subtree_sessions(agents, root) {
        heal_from_predecessor(holly, pending, &session, |_| true);
    }
}

/// The heal the decision router runs before a `Stop` or a decision: a
/// `Stop` moves silently, and a decision announces everything but the card
/// it answers — announcing a card about to be unwound or answered would
/// show a dead one.
pub(crate) fn heal_before(holly: &Holly, pending: &PendingDecisions, msg: &InMsg) {
    if let InMsg::Stop { session } = msg {
        heal_from_predecessor(holly, pending, session, |_| false);
    } else if let Some((session, request_id)) = Decision::target(msg) {
        // Only the card this decision answers on its own session goes
        // unannounced; one moved elsewhere still needs its announce.
        heal_from_predecessor(holly, pending, session, |a| {
            a.request_id != request_id || &a.session != session
        });
    }
}

#[cfg(test)]
#[path = "successor_tests.rs"]
mod tests;
