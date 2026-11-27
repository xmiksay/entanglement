//! The session tree as the logs tell it (ADR-0212, ADR-0236).
//!
//! A session's parent is set by its `SessionStarted` and changed by each
//! `ParentChanged` a compaction logs when it moves the session (ADR-0234).
//! Each session's *latest* edge gives the tree now: the sessions a revert
//! waits on (the busy check), and the membership of a change that recorded
//! no `ancestry` (ADR-0236 §8).

use std::collections::{HashMap, HashSet, VecDeque};

use entanglement_core::{OutEvent, SessionId};

use crate::session_store::{LogPayload, LogRecord};

/// `target` plus every session under it now, by each session's latest parent
/// edge. Edges are merged across files by wall clock (record order within
/// one), since a cold-resumed child's later edges sit in another file.
pub(super) fn current_tree(
    files: &[(SessionId, Vec<LogRecord>)],
    target: &SessionId,
) -> HashSet<SessionId> {
    let per_file: Vec<Vec<(&SessionId, &SessionId, u64)>> = files
        .iter()
        .map(|(_, records)| records.iter().filter_map(edge).collect())
        .collect();
    let mut latest: HashMap<&SessionId, &SessionId> = HashMap::new();
    for (session, parent, _) in merge_by(per_file, |(_, _, ts)| *ts) {
        latest.insert(session, parent);
    }
    let mut children: HashMap<&SessionId, Vec<&SessionId>> = HashMap::new();
    for (session, parent) in latest {
        children.entry(parent).or_default().push(session);
    }
    let mut out = HashSet::from([target.clone()]);
    let mut queue = VecDeque::from([target]);
    while let Some(s) = queue.pop_front() {
        for child in children.get(s).into_iter().flatten() {
            if out.insert((*child).clone()) {
                queue.push_back(child);
            }
        }
    }
    out
}

/// A parent edge `(session, parent, ts)`, if `r` is one.
fn edge(r: &LogRecord) -> Option<(&SessionId, &SessionId, u64)> {
    match &r.payload {
        LogPayload::Out(OutEvent::SessionStarted {
            session,
            parent: Some(parent),
            ..
        })
        | LogPayload::Out(OutEvent::ParentChanged { session, parent }) => {
            Some((session, parent, r.ts))
        }
        _ => None,
    }
}

/// Merge `lists` by `key`, keeping each list's own order: the head with the
/// smallest key goes next, the earlier list on a tie. Every list is already
/// in record order, which the wall clock must never reorder; only *between*
/// lists, which share no positions, does the clock decide.
pub(super) fn merge_by<T>(lists: Vec<Vec<T>>, key: impl Fn(&T) -> u64) -> Vec<T> {
    let mut iters: Vec<_> = lists
        .into_iter()
        .map(|l| l.into_iter().peekable())
        .collect();
    let mut out = Vec::new();
    loop {
        let next = iters
            .iter_mut()
            .enumerate()
            .filter_map(|(i, it)| it.peek().map(|head| (key(head), i)))
            .min();
        match next.and_then(|(_, i)| iters.get_mut(i)?.next()) {
            Some(item) => out.push(item),
            None => return out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_each_lists_order_and_interleaves_by_key() {
        let merged = merge_by(vec![vec![5, 1, 9], vec![2, 6]], |x| *x);
        assert_eq!(merged, vec![2, 5, 1, 6, 9], "list order is never re-sorted");
    }
}
