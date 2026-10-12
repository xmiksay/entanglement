//! The spawn sub-tree walk shared by the pending-decision machinery
//! (`crate::pending::park`): which sessions descend from a root.

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
            for child in every_child
                .iter()
                .filter(|c| &c.session == parent)
                .map(|c| SessionId::new(c.handle.clone()))
            {
                if seen.insert(child.clone()) {
                    next.push(child);
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
}
