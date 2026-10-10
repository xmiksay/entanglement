//! Spawn-tree walks, sub-tree hibernation and the idle-TTL sweep (#363).

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::holly::ActivityRegistry;
use crate::protocol::{SessionId, SessionInfo};
use crate::session::SessionCmd;

/// Follow the compaction-successor chain from `session` to the live session
/// that inherited its turn (ADR-0205). A long-running session can compact
/// repeatedly, so a result dispatched before an early fork must walk the whole
/// chain, not one link. Bounded by the map size so a cycle (which the
/// single-use id rule already prevents) can't spin.
pub(super) fn successor_of(
    successors: &HashMap<SessionId, SessionId>,
    session: &SessionId,
) -> Option<SessionId> {
    let mut current = successors.get(session)?.clone();
    for _ in 0..successors.len() {
        match successors.get(&current) {
            Some(next) => current = next.clone(),
            None => break,
        }
    }
    Some(current)
}

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

/// Evict `root` plus its whole spawn sub-tree: tear down each live task, drop
/// its `session_meta` entry, and record **no** tombstone so every id stays
/// resumable (#318, ADR-0077). Shared by the `HibernateSession` handler and the
/// idle-TTL sweep (#363) — the only two paths that decide a session should be
/// evicted.
///
/// `parent_links` is deliberately **not** torn down here (issue #639): it's
/// the only record of "this id is a sub-agent child" that survives eviction,
/// and the lazy-`Prompt` path in the main loop consults it precisely so a
/// hibernated child can't be silently blank-respawned under the most-
/// privileged default profile. It's still cleaned up on `CloseSession`, so a
/// tombstoned id doesn't linger forever.
pub(super) async fn hibernate_subtree(
    root: &SessionId,
    sessions: &mut HashMap<SessionId, mpsc::Sender<SessionCmd>>,
    session_meta: &mut HashMap<SessionId, SessionInfo>,
    parent_links: &mut HashMap<SessionId, Option<SessionId>>,
) {
    for victim in collect_subtree(root, parent_links) {
        if let Some(tx) = sessions.remove(&victim) {
            // Deliver `Hibernate`, then drop the sender: a buffered command is
            // received before the channel-closed `None`, so the task tears down
            // via `SessionHibernated`. The drop also unblocks a turn parked
            // mid-stream — its `rx.recv()` returns `None` (cancel), then the
            // stashed `Hibernate` pops when idle (stop-then-hibernate). Awaiting
            // is safe: the session drains its own channel independently of this
            // loop.
            let _ = tx.send(SessionCmd::Hibernate).await;
        }
        session_meta.remove(&victim);
        // Deliberately NOT inserted into `closed`: the id is evictable and
        // rebuildable, never spent.
    }
}

/// Coarse polling period for the idle-TTL sweep (#363): a quarter of the TTL,
/// floored at 30s so a short TTL doesn't spin the supervisor loop. This is an
/// eviction policy, not a scheduler — a session can sit idle up to one extra
/// sweep period past `idle_ttl` before it's noticed.
pub(super) fn sweep_period(ttl: Duration) -> Duration {
    (ttl / 4).max(Duration::from_secs(30))
}

/// One idle-TTL sweep tick (#363): auto-hibernate every **settled** root whose
/// whole spawn sub-tree has been continuously idle for at least `ttl`.
/// Judged per root, strictly — a live turn or a single parked child anywhere in
/// the sub-tree pins the whole tree live, matching manual `HibernateSession`'s
/// stricter-than-`Stop` posture but going further: unlike a manual hibernate
/// (stop-then-hibernate), a timer must never cancel live work, so this sweep
/// only ever touches a session already at rest. The idle clock for a sub-tree
/// starts at the *latest* member's settle time — a recently-active child resets
/// the whole tree's window, not just its own.
pub(super) async fn sweep_idle_sessions(
    ttl: Duration,
    sessions: &mut HashMap<SessionId, mpsc::Sender<SessionCmd>>,
    session_meta: &mut HashMap<SessionId, SessionInfo>,
    parent_links: &mut HashMap<SessionId, Option<SessionId>>,
    activity: &ActivityRegistry,
) {
    let now = tokio::time::Instant::now();
    let roots: Vec<SessionId> = session_meta
        .values()
        .filter(|m| m.root)
        .map(|m| m.session.clone())
        .collect();
    for root in roots {
        if !sessions.contains_key(&root) {
            continue;
        }
        let subtree = collect_subtree(&root, parent_links);
        let settled_since = {
            let idle = activity.lock().expect("activity registry mutex poisoned");
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
            hibernate_subtree(&root, sessions, session_meta, parent_links).await;
        }
    }
}
