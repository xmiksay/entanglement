//! Spawn-tree walks, sub-tree close and hibernation (with the exit signal a
//! hibernated task leaves for its successor task, #892), and the idle-TTL
//! sweep (#363).

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{unknown, Supervisor};
use crate::protocol::SessionId;
use crate::session::{ExitSignal, SessionCmd};

/// Collect `root` plus every transitive descendant from the child→parent
/// `parent_links` map (breadth-first). Backs the `CloseSession` cascade (#180):
/// closing a session must retire its whole sub-tree so no spawned sub-agent is
/// left orphaned. Small session counts make the O(n²) scan a non-issue.
pub(super) fn collect_subtree(
    root: &SessionId,
    parent_links: &HashMap<SessionId, Option<SessionId>>,
) -> Vec<SessionId> {
    let mut subtree = vec![root.clone()];
    let mut cursor = 0;
    while cursor < subtree.len() {
        let current = subtree[cursor].clone();
        for (child, parent) in parent_links {
            if parent.as_ref() == Some(&current) && !subtree.contains(child) {
                subtree.push(child.clone());
            }
        }
        cursor += 1;
    }
    subtree
}

impl Supervisor {
    /// Evict `root` plus its whole spawn sub-tree: tear down each live task,
    /// drop its `session_meta` entry, and record **no** tombstone so every id
    /// stays resumable (#318, ADR-0077). Shared by the `HibernateSession`
    /// handler and the idle-TTL sweep (#363) — the only two paths that decide
    /// a session should be evicted.
    ///
    /// `parent_links` is deliberately **not** torn down here (issue #639): it's
    /// the only record of "this id is a sub-agent child" that survives eviction,
    /// and the lazy-`Prompt` path in the main loop consults it precisely so a
    /// hibernated child can't be silently blank-respawned under the most-
    /// privileged default profile. It's still cleaned up on `CloseSession`, so a
    /// tombstoned id doesn't linger forever.
    pub(super) async fn hibernate_subtree(&mut self, root: &SessionId) {
        for victim in collect_subtree(root, &self.parent_links) {
            if let Some(tx) = self.sessions.remove(&victim) {
                // Deliver `Hibernate`, then drop the sender: a buffered command is
                // received before the channel-closed `None`, so the task tears down
                // via `SessionHibernated`. The drop also unblocks a turn parked
                // mid-stream — its `rx.recv()` returns `None` (cancel), then the
                // stashed `Hibernate` pops when idle (stop-then-hibernate). Awaiting
                // is safe: the session drains its own channel independently of this
                // loop. A task still waiting for its own predecessor (ADR-0244)
                // buffers it like any other command, as a live session busy in
                // an await that doesn't watch its inbox does.
                let _ = tx.send(SessionCmd::Hibernate).await;
                if let Some(task) = self.tasks.remove(&victim) {
                    self.await_retire(victim.clone(), task);
                }
            }
            self.session_meta.remove(&victim);
            // Deliberately NOT inserted into `closed`: the id is evictable and
            // rebuildable, never spent.
        }
    }

    /// Publish `session`'s exit signal while its old task tears down (#892,
    /// ADR-0244). A task started for the id meanwhile (a `Resume`, a lazy
    /// spawn, a load) takes the signal and waits on it before it does
    /// anything, so two tasks never run for one id; the supervisor itself
    /// never waits. The watcher reports the exit back so the entry is
    /// dropped, unless a newer task under the id took its place.
    fn await_retire(&mut self, session: SessionId, task: JoinHandle<()>) {
        let (exited, signal) = watch::channel(false);
        self.retiring.insert(session.clone(), signal.clone());
        let done = self.retired_tx.clone();
        tokio::spawn(async move {
            // A panicked task has exited too: release its id either way.
            if let Err(e) = task.await {
                if e.is_panic() {
                    tracing::error!(%session, "session task panicked while hibernating");
                }
            }
            exited.send_replace(true);
            let _ = done.send((session, signal)).await;
        });
    }

    /// `CloseSession`: retire `session` and its whole spawn sub-tree.
    pub(super) async fn close(&mut self, session: &SessionId) {
        // Cascade (#180): closing a session retires its whole sub-tree, not
        // just the target. `parent_links` is child→parent, so a spawned
        // descendant left running has no consumer for its answers and keeps
        // burning provider tokens. Walk the tree and close every descendant
        // alongside the target.
        // The closed sub-tree's root has a parent *outside* the closed set
        // (descendants' parents are themselves closing) — capture it before
        // the links are torn down so the still-live parent's `children`
        // mirror can drop the retired child.
        let root_parent = self.parent_links.get(session).cloned().flatten();
        for victim in collect_subtree(session, &self.parent_links) {
            // Dropping the command channel makes the task's `rx.recv()`
            // return `None`; it emits `SessionEnded` and exits. Unknown
            // id → no-op. The id is tombstoned below, so nothing can ever
            // start a second task for it: its join handle is just dropped, and
            // a still-retiring task's late `SessionHibernated` may follow. A
            // task still waiting for its predecessor exits without emitting.
            if self.sessions.remove(&victim).is_some() {
                self.session_meta.remove(&victim);
            }
            self.tasks.remove(&victim);
            self.parent_links.remove(&victim);
            self.modes.remove(&victim);
            self.workdirs.remove(&victim);
            // A load still in flight (ADR-0210) is abandoned: its queued
            // messages are refused like any frame racing a close, and the
            // completion finds no queue and applies nothing.
            if let Some(queued) = self.pending_loads.remove(&victim) {
                self.refuse_queued(&victim, queued.len(), unknown::CLOSED_ID);
            }
            // Tombstone the id regardless of liveness: it is spent
            // (ADR-0028), so a `Prompt` queued behind this `CloseSession`
            // can't respawn it blank.
            self.closed.insert(victim);
        }
        if let Some(parent) = root_parent {
            if let Some(ptx) = self.sessions.get(&parent) {
                let _ = ptx.send(SessionCmd::ChildClosed(session.clone())).await;
            }
        }
    }

    /// One idle-TTL sweep tick (#363): auto-hibernate every **settled** root
    /// whose whole spawn sub-tree has been continuously idle for at least
    /// `ttl`. Judged per root, strictly — a live turn or a single parked child
    /// anywhere in the sub-tree pins the whole tree live, matching manual
    /// `HibernateSession`'s stricter-than-`Stop` posture but going further:
    /// unlike a manual hibernate (stop-then-hibernate), a timer must never
    /// cancel live work, so this sweep only ever touches a session already at
    /// rest. The idle clock for a sub-tree starts at the *latest* member's
    /// settle time — a recently-active child resets the whole tree's window,
    /// not just its own.
    pub(super) async fn sweep_idle_sessions(&mut self, ttl: Duration) {
        let now = tokio::time::Instant::now();
        for root in self.session_meta.roots() {
            if !self.sessions.contains_key(&root) {
                continue;
            }
            let subtree = collect_subtree(&root, &self.parent_links);
            let settled_since = {
                let idle = self
                    .activity
                    .lock()
                    .expect("activity registry mutex poisoned");
                let mut latest: Option<tokio::time::Instant> = None;
                let mut all_settled = true;
                for member in &subtree {
                    match idle.get(member) {
                        Some(Some(t)) => latest = Some(latest.map_or(*t, |l| l.max(*t))),
                        _ => {
                            all_settled = false;
                            break;
                        }
                    }
                }
                all_settled.then_some(latest).flatten()
            };
            let Some(since) = settled_since else { continue };
            if now.saturating_duration_since(since) >= ttl {
                self.hibernate_subtree(&root).await;
            }
        }
    }
}

/// Coarse polling period for the idle-TTL sweep (#363): a quarter of the TTL,
/// floored at 30s so a short TTL doesn't spin the supervisor loop. This is an
/// eviction policy, not a scheduler — a session can sit idle up to one extra
/// sweep period past `idle_ttl` before it's noticed.
pub(super) fn sweep_period(ttl: Duration) -> Duration {
    (ttl / 4).max(Duration::from_secs(30))
}

/// `session`'s old task A has exited: drop its exit signal `signal`, unless a
/// newer task's signal already replaced it. That happens when A's watcher
/// report is handled late: B took A's signal and started as soon as A exited,
/// then a hibernate of B put B's own signal in the map — all before the
/// supervisor read A's report, which waits in the bounded `retired_tx`
/// channel while the loop is busy. Dropping B's signal then would let a
/// third task start beside B's teardown (ADR-0244 §4).
pub(super) fn release(
    retiring: &mut HashMap<SessionId, ExitSignal>,
    session: SessionId,
    signal: ExitSignal,
) {
    if retiring
        .get(&session)
        .is_some_and(|s| s.same_channel(&signal))
    {
        retiring.remove(&session);
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    use super::*;

    #[test]
    fn a_late_report_keeps_the_newer_tasks_signal() {
        let id = SessionId::new("s");
        let (_a, sa) = watch::channel(false);
        let (_b, sb) = watch::channel(false);
        let mut retiring = HashMap::from([(id.clone(), sb.clone())]);
        release(&mut retiring, id.clone(), sa);
        assert!(
            retiring[&id].same_channel(&sb),
            "A's report keeps B's signal"
        );
        release(&mut retiring, id.clone(), sb);
        assert!(retiring.is_empty(), "B's own report drops it");
    }
}
