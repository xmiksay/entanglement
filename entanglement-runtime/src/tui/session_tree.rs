//! Spawn-tree ordering for session lists (sidebar + sessions modal).
//!
//! The registry's insertion order interleaves spawned children with unrelated
//! roots (a child is auto-discovered whenever its first event arrives), so a
//! flat listing scatters a session's sub-agents. [`tree_order`] re-orders an
//! insertion-ordered slice depth-first — every session appears directly under
//! its parent — and [`get_depth`] (moved from `modals.rs`) gives the indent.

use entanglement_core::SessionId;
use std::collections::HashMap;

pub type ParentLinks = HashMap<SessionId, Option<SessionId>>;

/// Depth of `id` in the spawn tree by walking parent links. Bounded at 100 hops
/// so a corrupt cyclic link map (a session that is transitively its own parent)
/// terminates instead of spinning forever.
pub fn get_depth(id: &SessionId, parent_links: &ParentLinks) -> usize {
    let mut depth = 0;
    let mut current = id;
    while let Some(parent) = parent_links.get(current).and_then(|p| p.as_ref()) {
        depth += 1;
        current = parent;
        if depth > 100 {
            break;
        }
    }
    depth
}

/// Indices into `ids` in depth-first spawn-tree order: roots (and sessions
/// whose parent is unknown to the map — matching [`get_depth`]'s unknown ⇒ 0)
/// keep their given order, each immediately followed by its descendants,
/// children in given order. A corrupt parent cycle leaves its members
/// unreachable from any root; they are appended at the end in given order —
/// a session is never dropped from the listing.
pub fn tree_order(ids: &[&SessionId], links: &ParentLinks) -> Vec<usize> {
    let index: HashMap<&SessionId, usize> =
        ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let parent_idx = links
            .get(*id)
            .and_then(|p| p.as_ref())
            .and_then(|p| index.get(p).copied());
        match parent_idx {
            Some(pi) if pi != i => children[pi].push(i),
            _ => roots.push(i),
        }
    }
    let mut out = Vec::with_capacity(ids.len());
    let mut visited = vec![false; ids.len()];
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(i) = stack.pop() {
        if visited[i] {
            continue;
        }
        visited[i] = true;
        out.push(i);
        for &c in children[i].iter().rev() {
            stack.push(c);
        }
    }
    for (i, seen) in visited.iter().enumerate() {
        if !seen {
            out.push(i);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(pairs: &[(&str, Option<&str>)]) -> ParentLinks {
        pairs
            .iter()
            .map(|(id, parent)| (SessionId::new(*id), parent.map(SessionId::new)))
            .collect()
    }

    fn order<'a>(ids: &[&'a SessionId], map: &ParentLinks) -> Vec<&'a SessionId> {
        tree_order(ids, map).into_iter().map(|i| ids[i]).collect()
    }

    #[test]
    fn depth_of_root_is_zero() {
        let map = links(&[("root", None)]);
        assert_eq!(get_depth(&SessionId::new("root"), &map), 0);
    }

    #[test]
    fn depth_counts_the_parent_chain() {
        let map = links(&[
            ("root", None),
            ("child", Some("root")),
            ("grandchild", Some("child")),
        ]);
        assert_eq!(get_depth(&SessionId::new("grandchild"), &map), 2);
        assert_eq!(get_depth(&SessionId::new("child"), &map), 1);
    }

    #[test]
    fn unknown_id_is_depth_zero() {
        let map = links(&[("root", None)]);
        assert_eq!(get_depth(&SessionId::new("ghost"), &map), 0);
    }

    #[test]
    fn cyclic_links_terminate_via_the_hop_guard() {
        // A corrupt map where two sessions are each other's parent must not spin.
        let map = links(&[("a", Some("b")), ("b", Some("a"))]);
        assert_eq!(get_depth(&SessionId::new("a"), &map), 101);
    }

    #[test]
    fn children_nest_under_their_parent_despite_interleaved_registration() {
        // Registry order: root1, root2, child-of-root1 (discovered last).
        let (r1, r2, c1) = (
            SessionId::new("r1"),
            SessionId::new("r2"),
            SessionId::new("c1"),
        );
        let map = links(&[("r1", None), ("r2", None), ("c1", Some("r1"))]);
        assert_eq!(order(&[&r1, &r2, &c1], &map), vec![&r1, &c1, &r2]);
    }

    #[test]
    fn descendants_are_depth_first_in_given_order() {
        let ids: Vec<SessionId> = ["r", "a", "b", "aa"]
            .iter()
            .map(|s| SessionId::new(*s))
            .collect();
        let map = links(&[
            ("r", None),
            ("a", Some("r")),
            ("b", Some("r")),
            ("aa", Some("a")),
        ]);
        let refs: Vec<&SessionId> = ids.iter().collect();
        let got: Vec<&str> = order(&refs, &map)
            .into_iter()
            .map(|id| id.0.as_str())
            .collect();
        assert_eq!(got, vec!["r", "a", "aa", "b"]);
    }

    #[test]
    fn unknown_parent_is_listed_as_a_root() {
        // A child whose parent never registered a view (or was pruned) must
        // still be listed — as a root, matching get_depth's unknown ⇒ 0.
        let (r, orphan) = (SessionId::new("r"), SessionId::new("orphan"));
        let map = links(&[("r", None), ("orphan", Some("ghost"))]);
        assert_eq!(order(&[&r, &orphan], &map), vec![&r, &orphan]);
    }

    #[test]
    fn cycle_members_are_appended_never_dropped() {
        let ids: Vec<SessionId> = ["r", "x", "y"].iter().map(|s| SessionId::new(*s)).collect();
        let map = links(&[("r", None), ("x", Some("y")), ("y", Some("x"))]);
        let refs: Vec<&SessionId> = ids.iter().collect();
        let got: Vec<&str> = order(&refs, &map)
            .into_iter()
            .map(|id| id.0.as_str())
            .collect();
        assert_eq!(got.len(), 3, "no session may be dropped");
        assert_eq!(got, vec!["r", "x", "y"]);
    }
}
