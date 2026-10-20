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
//! answers or stops the successor's cards. Every move walks the whole
//! compaction chain back from its target (`Holly::predecessors_of`,
//! ADR-0242), so two compactions before a fold or heal still reach the
//! oldest ancestor's cards.

use entanglement_core::{Holly, InMsg, SessionId};
use tokio::sync::oneshot;

use super::{Inner, Parked, PendingDecisions};
use crate::agent_registry::AgentRegistry;
use crate::seam::Decision;

type Key = (SessionId, String);

impl Inner {
    /// Insert under `key`, home `parked.home`. Only a turn card supersedes a
    /// live turn card under the same key, stopping it (a resume's re-offer
    /// after a lagged-away `SessionHibernated`), as before ADR-0238. Any other
    /// park sharing a live card's key, or its id on its home, is refused:
    /// `Err` carries an already-rejected receiver, so the call fails with
    /// the reason, and a live detached card is never displaced. `Ok` also
    /// returns the superseded card, which the caller reports resolved.
    pub(super) fn insert(
        &mut self,
        session: &SessionId,
        request_id: &str,
        parked: Parked,
    ) -> Result<(oneshot::Receiver<Decision>, Option<Parked>), oneshot::Receiver<Decision>> {
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
        let superseded = self.waiters.insert(key, (tx, parked)).map(|(old, card)| {
            let _ = old.send(Decision::Stop);
            card
        });
        Ok((rx, superseded))
    }

    /// The key a decision for `(session, request_id)` answers, among the
    /// cards [`answers`] matches: a live one over any that gave up, then the
    /// exact key, then the key string as a deterministic last tie-break.
    /// At most one live foreign candidate exists: a card leaves a home only
    /// when that session retires, and a park or a move onto a live card's id
    /// is refused or skipped.
    fn find(&self, session: &SessionId, request_id: &str) -> Option<Key> {
        let mut candidates: Vec<(&Key, bool, bool)> = self
            .waiters
            .iter()
            .filter(|(k, (_, p))| answers(k, p, session, request_id))
            .map(|(k, (tx, _))| (k, tx.is_closed(), &k.0 != session))
            .collect();
        candidates.sort_by_key(|(k, closed, foreign)| (*closed, *foreign, k.0 .0.clone()));
        candidates.first().map(|(k, _, _)| (*k).clone())
    }

    /// A timed-out park's own entry, by its exact key and only once its
    /// waiter is gone: a live card moved onto that id is someone else's.
    pub(super) fn forget(&mut self, session: &SessionId, request_id: &str) -> Option<Parked> {
        let key = (session.clone(), request_id.to_string());
        if self.waiters.get(&key).is_some_and(|(tx, _)| tx.is_closed()) {
            return self.waiters.remove(&key).map(|(_, parked)| parked);
        }
        None
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

/// Whether a decision for `(session, request_id)` reaches the card under
/// `key`: its exact key, or a card shown on `session` now (its home) or
/// before a move took it on (`via`) — a stale copy a head may still answer
/// (ADR-0242). A card is never shown upstream of where it was parked, so a
/// predecessor's id never reaches a card the successor parked itself.
fn answers(key: &Key, parked: &Parked, session: &SessionId, request_id: &str) -> bool {
    key.1 == request_id
        && (&key.0 == session || &parked.home == session || parked.via.contains(session))
}

impl PendingDecisions {
    /// Move every detached card shown on one of `ancestors` (nearest first)
    /// to `target`, and call `emit` with each still-waiting moved approval —
    /// under the lock, so no decision can resolve a card between its move
    /// and its announcement (`emit` sees every live moved card).
    /// Each card remembers the home it left (`via`). Idempotent. Turn cards
    /// stay: their turn ends with their session (`stop_attached`). A card
    /// whose id the target already shows stays where it is, unannounced: a
    /// home never holds two live cards with one id, so of two ancestors'
    /// cards with one id only the nearest moves. Parks heal first, so that is a narrow race.
    pub(crate) fn reassign_detached(
        &self,
        ancestors: &[SessionId],
        target: &SessionId,
        mut emit: impl FnMut(&Key, &Parked),
    ) {
        let mut guard = self.lock();
        let mut shown: std::collections::HashSet<String> = guard
            .waiters
            .iter()
            .filter(|(_, (tx, p))| &p.home == target && !tx.is_closed())
            .map(|((_, rid), _)| rid.clone())
            .collect();
        for from in ancestors.iter().filter(|a| *a != target) {
            for (key, (tx, parked)) in guard.waiters.iter_mut() {
                if &parked.home != from || !parked.detached || shown.contains(&key.1) {
                    continue;
                }
                parked
                    .via
                    .push(std::mem::replace(&mut parked.home, target.clone()));
                if let Some(approval) = parked.approval.as_mut() {
                    approval.session = target.clone();
                }
                if !tx.is_closed() {
                    shown.insert(key.1.clone());
                    emit(key, parked);
                }
            }
        }
    }
}

/// Where `session`'s detached work belongs now: the last link of its
/// compaction chain (`Holly::successor_of` already walks past retired middle
/// links) when that is live, else `session` itself. A successor that is not
/// live yet has no seq counter, so an announce there would carry seq 0; its
/// `SessionStarted` fold moves the work with a real seq. The same notion a
/// background script follows (`Dispatcher::followable`, ADR-0241).
pub(crate) fn live_end(holly: &Holly, session: &SessionId) -> SessionId {
    holly
        .successor_of(session)
        .filter(|s| holly.is_live(s))
        .unwrap_or_else(|| session.clone())
}

/// Announce a moved card on its new home: a fresh `ToolRequest`, same
/// request id and content, `detached: true`, so a head moves it there.
pub(crate) fn announce(holly: &Holly, parked: &Parked) {
    let Some(approval) = &parked.approval else {
        return;
    };
    tracing::debug!(
        session = %approval.session,
        request_id = %approval.request_id,
        "moved a background script's card to the compaction successor"
    );
    super::park::emit_request(holly, approval);
}

/// The lossless self-heal for a live `session`: pull every detached card of
/// its lineage — the whole chain as the supervisor's directory records it,
/// not one hop (ADR-0242) — onto its live end ([`live_end`]), announcing the
/// moved cards `announce_if` accepts. A no-op for a session that is not live
/// (an ended one is no target, and a starting one is the fold's), that is
/// no compaction successor, or whose move already ran.
pub(crate) fn heal_lineage(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    announce_if: impl Fn(&Key, &Parked) -> bool,
) {
    if holly.is_live(session) {
        move_to_live_end(holly, pending, session, announce_if);
    }
}

/// The heal a park runs before registering (ADR-0238 §2): `session`'s whole
/// lineage onto its live end, even from a retired `session`, so the
/// successor's own park meets a moved card already on it (and is refused,
/// never the reverse), and a late predecessor park lands beside its moved
/// siblings.
pub(super) fn heal_for_park(holly: &Holly, pending: &PendingDecisions, session: &SessionId) {
    move_to_live_end(holly, pending, session, |_, _| true);
}

fn move_to_live_end(
    holly: &Holly,
    pending: &PendingDecisions,
    session: &SessionId,
    announce_if: impl Fn(&Key, &Parked) -> bool,
) {
    let target = live_end(holly, session);
    // The start-up window: the directory already names the predecessor, but
    // the seq counter does not exist yet. A move now would announce with
    // seq 0 (dropped by heads) and leave the `SessionStarted` fold nothing
    // to announce, losing the card; the fold moves it once it is live.
    if !holly.is_live(&target) {
        return;
    }
    let ancestors = holly.predecessors_of(&target);
    pending.reassign_detached(&ancestors, &target, |k, p| {
        if announce_if(k, p) {
            announce(holly, p);
        }
    });
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
        heal_lineage(holly, pending, &session, |_, _| true);
    }
}

/// The heal the decision router runs before a `Stop` or a decision: a
/// `Stop` moves silently, and a decision announces every moved card but the
/// ones it answers ([`answers`]) — announcing a card about to be unwound or
/// answered would show a dead one.
pub(crate) fn heal_before(holly: &Holly, pending: &PendingDecisions, msg: &InMsg) {
    if let InMsg::Stop { session } = msg {
        heal_lineage(holly, pending, session, |_, _| false);
    } else if let Some((session, request_id)) = Decision::target(msg) {
        heal_lineage(holly, pending, session, |k, p| {
            !answers(k, p, session, request_id)
        });
    }
}

#[cfg(test)]
#[path = "successor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "successor_chain_tests.rs"]
mod chain_tests;
