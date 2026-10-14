//! The spawn sub-tree walk shared by the pending-decision machinery
//! (`crate::pending::park`) and a handle-less `poll`'s subtree section
//! (`crate::poll`, #829): which sessions descend from a root.

use std::collections::HashSet;

use entanglement_core::SessionId;

use crate::agent_registry::AgentRegistry;

/// Every session in `root`'s spawn sub-tree, `root` included: a breadth-first
/// walk of [`AgentRegistry`]'s parent→child links (each spawn registers a
/// child under its immediate parent, so a grandchild is only reachable by
/// walking one level at a time). Bounded by construction — a mode's
/// `max_depth`/`max_agents` already caps how wide/deep the live tree can be —
/// but capped defensively at a fixed iteration count too, so a bug elsewhere
/// can never turn this into an unbounded loop.
const MAX_WALK_STEPS: usize = 4096;

pub(crate) fn subtree_sessions(agents: &AgentRegistry, root: &SessionId) -> HashSet<SessionId> {
    let every_child = agents.snapshot(None);
    let mut seen: HashSet<SessionId> = HashSet::from([root.clone()]);
    let mut frontier: Vec<SessionId> = vec![root.clone()];
    let mut steps = 0;
    while !frontier.is_empty() && steps < MAX_WALK_STEPS {
        let mut next = Vec::new();
        for parent in &frontier {
            for c in every_child.iter().filter(|c| &c.session == parent) {
                // A compacted child is reached under its handle and walked
                // from its live session, which is what owns its own
                // sub-agents and parks its approvals (ADR-0234).
                let handle_new = seen.insert(SessionId::new(c.handle.clone()));
                let live_new = seen.insert(c.child_session.clone());
                if handle_new || live_new {
                    next.push(c.child_session.clone());
                }
            }
            steps += 1;
        }
        frontier = next;
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walk_reaches_grandchildren_and_skips_siblings() {
        let agents = AgentRegistry::default();
        let (root, child, grandchild, sibling) = (
            SessionId::new("root"),
            SessionId::new("child"),
            SessionId::new("grandchild"),
            SessionId::new("sibling"),
        );
        agents.register(child.clone(), root.clone(), "a".into());
        agents.register(grandchild.clone(), child.clone(), "b".into());
        agents.register(sibling.clone(), SessionId::new("other"), "c".into());
        let tree = subtree_sessions(&agents, &root);
        assert!(tree.contains(&root) && tree.contains(&child) && tree.contains(&grandchild));
        assert!(!tree.contains(&sibling));
    }

    /// ADR-0234: a child that compacted is walked from its live successor,
    /// so the successor's own sub-agents stay in the tree.
    #[test]
    fn the_walk_follows_a_compacted_child_to_its_successor() {
        let agents = AgentRegistry::default();
        let (root, child, succ, grandchild) = (
            SessionId::new("root"),
            SessionId::new("child"),
            SessionId::new("child-succ"),
            SessionId::new("grandchild"),
        );
        agents.register(child.clone(), root.clone(), "a".into());
        agents.follow_successor(&child, &succ);
        agents.register(grandchild.clone(), succ.clone(), "b".into());
        let tree = subtree_sessions(&agents, &root);
        for id in [&root, &child, &succ, &grandchild] {
            assert!(tree.contains(id), "{id} missing from {tree:?}");
        }
    }
}
