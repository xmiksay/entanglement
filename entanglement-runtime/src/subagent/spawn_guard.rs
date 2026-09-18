//! Bounding sub-agent spawning by the session's permission mode (#76,
//! ADR-0023; mode-sourced since ADR-0207 §6).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use entanglement_core::SessionId;

/// Tracks the live session tree so the runtime can bound sub-agent spawning:
/// depth is `Mode::limits.max_depth`, and `Mode::limits.max_agents` caps how
/// many sub-agents run **at once** anywhere in one root's tree. Fed each
/// `SessionStarted` (for the parent link) and consulted on every `agent` call
/// before a child is started. Lives in the tool executor's single-threaded
/// event loop, so only the per-root running count — released from the
/// spawned child's own task — needs to be shared.
///
/// ADR-0207 §7 retires the sponsored-child concept (ADR-0138): approving a
/// `propose_plan` now switches the session's mode instead of spawning a
/// permission-root child, so [`crate::permission::ancestor_chain`]'s clamp
/// (ADR-0024) has no exemption left to consult here.
#[derive(Default)]
pub struct SpawnGuard {
    /// child → parent, from `SessionStarted`. Absent or `None` ⇒ a root.
    parents: HashMap<SessionId, Option<SessionId>>,
    /// root → sub-agents currently running beneath it. A cumulative count
    /// would exhaust a long session that delegates one task at a time, which
    /// is not what the cap guards against — fan-out pressure on the endpoint
    /// and the tree is about concurrency, so each [`SpawnSlot`] gives its
    /// place back when the child's launch task ends.
    running_per_root: HashMap<SessionId, Arc<AtomicUsize>>,
}

/// One running sub-agent's place in its root's [`SpawnGuard`] budget, held by
/// the task driving the child's launch (blocking or `background: true`, both
/// of which live until the child's answer arrives) and released on drop — so
/// a child that errors, is refused by the engine, or whose task panics can
/// never leak a slot.
#[derive(Debug)]
pub struct SpawnSlot(Arc<AtomicUsize>);

impl Drop for SpawnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SpawnGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a session's parent from its `SessionStarted` event.
    pub fn record_start(&mut self, session: SessionId, parent: Option<SessionId>) {
        self.parents.insert(session, parent);
    }

    /// The recorded parent of `session`, if any. Lets the tool executor walk a
    /// child's ancestry to clamp its permissions to the parent chain (#77).
    pub fn parent_of(&self, session: &SessionId) -> Option<SessionId> {
        self.parents.get(session).cloned().flatten()
    }

    /// Decide whether `parent` may spawn another sub-agent under `mode`
    /// (ADR-0207 §6: mode applies to the whole spawn sub-tree, so the
    /// spawning session's own mode is always the right one to bound by — no
    /// per-spawn override exists). On approval, returns the [`SpawnSlot`] the
    /// caller must hold for as long as the child runs. On refusal, returns the
    /// message to relay to the parent as the `agent` tool output, naming the
    /// limit and the mode. `None` in either `Limits` field means unlimited
    /// (ADR-0207 §6: "undefined means unlimited").
    ///
    /// The slot is taken here, synchronously, not when the child's
    /// `SessionStarted` arrives: a batch of `background: true` spawns is
    /// dispatched back to back before any child has started, and every one of
    /// them must already see its siblings counted.
    pub fn try_spawn(
        &mut self,
        parent: &SessionId,
        mode: &crate::mode::Mode,
    ) -> Result<SpawnSlot, String> {
        let child_depth = self.depth(parent) + 1;
        if let Some(max_depth) = mode.limits.max_depth {
            if child_depth as u32 > max_depth {
                return Err(format!(
                    "sub-agent spawn refused: max spawn depth ({max_depth}) reached for mode \
                     '{}' — this sub-agent is too deeply nested to spawn another. Do the work \
                     directly.",
                    mode.name
                ));
            }
        }
        let root = self.root_of(parent);
        let running = self.running_per_root.entry(root).or_default().clone();
        if let Some(max_agents) = mode.limits.max_agents {
            if running.load(Ordering::Relaxed) as u32 >= max_agents {
                return Err(format!(
                    "sub-agent spawn refused: {max_agents} sub-agents are already running in \
                     this session tree, the most mode '{}' allows at once. Wait for one to \
                     finish (poll its agent_id) and spawn again, or do the work directly.",
                    mode.name
                ));
            }
        }
        running.fetch_add(1, Ordering::Relaxed);
        Ok(SpawnSlot(running))
    }

    /// Number of ancestors of `session` (a root is depth 0). The `visited` set
    /// guards against a malformed cycle in the parent links.
    fn depth(&self, session: &SessionId) -> usize {
        let mut depth = 0;
        let mut current = session.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            match self.parents.get(&current).cloned().flatten() {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => break,
            }
        }
        depth
    }

    /// Walk to the root of `session`'s tree (itself if it has no parent).
    fn root_of(&self, session: &SessionId) -> SessionId {
        let mut current = session.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            match self.parents.get(&current).cloned().flatten() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        current
    }

    #[cfg(test)]
    fn running(&self, root: &SessionId) -> Option<usize> {
        self.running_per_root
            .get(root)
            .map(|c| c.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a guard with a linear ancestry chain `root → a → b → …` recorded.
    fn guard_with_chain(chain: &[&str]) -> (SpawnGuard, Vec<SessionId>) {
        let mut guard = SpawnGuard::new();
        let ids: Vec<SessionId> = chain.iter().map(|c| SessionId::new(*c)).collect();
        for (i, id) in ids.iter().enumerate() {
            let parent = i.checked_sub(1).map(|p| ids[p].clone());
            guard.record_start(id.clone(), parent);
        }
        (guard, ids)
    }

    /// A mode with the given depth/concurrency limits (`None` = unlimited),
    /// otherwise a bare pass-through — `try_spawn` only ever reads `name`
    /// and `limits`.
    fn mode_with_limits(max_depth: Option<u32>, max_agents: Option<u32>) -> crate::mode::Mode {
        crate::mode::Mode {
            name: "test".to_string(),
            default: entanglement_core::Permission::Allow,
            rules: crate::mode::Rules::default(),
            limits: crate::mode::Limits {
                max_depth,
                max_agents,
                ..Default::default()
            },
            sandbox: None,
            sandbox_network: false,
        }
    }

    #[test]
    fn a_held_slot_counts_as_running() {
        let (mut guard, ids) = guard_with_chain(&["root"]);
        let mode = mode_with_limits(Some(3), Some(16));
        let _slot = guard.try_spawn(&ids[0], &mode).unwrap();
        assert_eq!(guard.running(&ids[0]), Some(1));
    }

    #[test]
    fn spawn_refused_past_max_depth() {
        // root(0) → a(1) → b(2) → c(3): c is at the mode's max_depth (3), so
        // its spawn (which would be depth 4) is refused.
        let (mut guard, ids) = guard_with_chain(&["root", "a", "b", "c"]);
        let mode = mode_with_limits(Some(3), Some(16));
        let deepest = ids.last().unwrap();
        let err = guard.try_spawn(deepest, &mode).unwrap_err();
        assert!(err.contains("max spawn depth"), "got: {err}");
        assert!(err.contains("test"), "names the mode: {err}");
        // A shallower ancestor (depth 2 → child depth 3) is still allowed.
        assert!(guard.try_spawn(&ids[2], &mode).is_ok());
    }

    #[test]
    fn spawn_refused_while_the_limit_is_running() {
        let (mut guard, ids) = guard_with_chain(&["root"]);
        let mode = mode_with_limits(Some(3), Some(4));
        let slots: Vec<SpawnSlot> = (0..4)
            .map(|_| guard.try_spawn(&ids[0], &mode).unwrap())
            .collect();
        let err = guard.try_spawn(&ids[0], &mode).unwrap_err();
        assert!(err.contains("already running"), "got: {err}");
        assert!(err.contains("test"), "names the mode: {err}");
        drop(slots);
    }

    /// The reported bug: a session delegating one task at a time hit a
    /// cumulative budget after `max_agents` sequential spawns, though never
    /// more than one ran at once.
    #[test]
    fn sequential_spawns_never_exhaust_the_limit() {
        let (mut guard, ids) = guard_with_chain(&["root"]);
        let mode = mode_with_limits(Some(3), Some(2));
        for _ in 0..50 {
            let slot = guard.try_spawn(&ids[0], &mode).unwrap();
            drop(slot);
        }
        assert_eq!(guard.running(&ids[0]), Some(0));
    }

    #[test]
    fn a_finished_child_frees_its_slot() {
        let (mut guard, ids) = guard_with_chain(&["root"]);
        let mode = mode_with_limits(Some(3), Some(2));
        let first = guard.try_spawn(&ids[0], &mode).unwrap();
        let _second = guard.try_spawn(&ids[0], &mode).unwrap();
        assert!(guard.try_spawn(&ids[0], &mode).is_err());
        drop(first);
        assert!(guard.try_spawn(&ids[0], &mode).is_ok());
    }

    #[test]
    fn undefined_limits_mean_unlimited() {
        // ADR-0207 §6: `None` in either field never refuses.
        let (mut guard, ids) = guard_with_chain(&["root", "a", "b", "c", "d", "e"]);
        let mode = mode_with_limits(None, None);
        let _slots: Vec<SpawnSlot> = ids
            .iter()
            .map(|id| guard.try_spawn(id, &mode).unwrap())
            .collect();
    }

    #[test]
    fn running_count_is_shared_across_the_whole_tree() {
        // A grandchild's running children count against the same root as the
        // root's own.
        let (mut guard, ids) = guard_with_chain(&["root", "child"]);
        let mode = mode_with_limits(Some(3), Some(16));
        let _a = guard.try_spawn(&ids[0], &mode).unwrap();
        let _b = guard.try_spawn(&ids[1], &mode).unwrap();
        assert_eq!(guard.running(&ids[0]), Some(2));
        assert_eq!(guard.running(&ids[1]), None);
    }

    #[test]
    fn unknown_session_treated_as_root() {
        let mut guard = SpawnGuard::new();
        let orphan = SessionId::new("orphan");
        let mode = mode_with_limits(Some(3), Some(16));
        // No `record_start`: depth 0, its own root — the spawn is allowed.
        assert!(guard.try_spawn(&orphan, &mode).is_ok());
    }
}
