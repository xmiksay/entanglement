//! Reading a root's whole live tree across files (ADR-0234).
//!
//! The tap logs a session into the root it started under for life, so a
//! sub-agent that core re-linked under a compaction successor keeps its
//! history in the predecessor's file. [`SessionStore::read_tree`] gathers it
//! back from two sources, both on disk only, so it works after a restart:
//! the [`LogPayload::Adopted`] pointers the tap writes into the adopting
//! root, followed recursively, and — for a pointer a crash never wrote — the
//! resumed root's immediate predecessor's tree (with its pointers resolved),
//! where a moved session's last `ParentChanged` names this tree.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use entanglement_core::{OutEvent, SessionId};

use super::{LogPayload, LogRecord, SessionStore};

impl SessionStore {
    /// Root `id`'s records plus every sub-tree adopted into it from another
    /// root's file, adopted records first. A session's records all come from
    /// one file in their written order, and an adoption always comes from an
    /// older file than the adopter, so each session's history stays in
    /// chronological order — which is all replay needs. Pointer rows are
    /// resolved and left out.
    ///
    /// A pointer whose file is gone (deleted or aged out) drops the adopted
    /// sub-tree from the result, including any records it wrote into `id`'s
    /// own file after a resume: resuming a session from a partial history
    /// would be worse than not resuming it.
    pub fn read_tree(&self, id: &SessionId) -> Result<Vec<LogRecord>> {
        let own = self.read(id)?;
        let mut reader = TreeReader {
            store: self,
            files: HashMap::from([(id.clone(), Some(own))]),
            visiting: HashSet::new(),
        };
        Ok(reader.tree(id, true).unwrap_or_default())
    }
}

/// One `read_tree` call: each file is read once (`files`), and `visiting`
/// breaks a pointer cycle a corrupted or hand-edited store could hold.
struct TreeReader<'a> {
    store: &'a SessionStore,
    files: HashMap<SessionId, Option<Vec<LogRecord>>>,
    visiting: HashSet<SessionId>,
}

impl TreeReader<'_> {
    fn file(&mut self, id: &SessionId) -> Option<Vec<LogRecord>> {
        if let Some(cached) = self.files.get(id) {
            return cached.clone();
        }
        let read = self
            .store
            .read(id)
            .map_err(|e| tracing::debug!(root = %id, "no readable log: {e:#}"))
            .ok();
        self.files.insert(id.clone(), read.clone());
        read
    }

    /// `None` when `id`'s file is unreadable or already on the path (a cycle).
    /// `resumed` marks the root being resumed, the only one whose
    /// predecessor is searched for a lost pointer.
    fn tree(&mut self, id: &SessionId, resumed: bool) -> Option<Vec<LogRecord>> {
        if self.visiting.contains(id) {
            return None;
        }
        let mut own = self.file(id)?;
        self.visiting.insert(id.clone());
        let mut tree = Vec::new();
        for source in self.sources(id, &own, resumed) {
            match self.tree(&source.from, false) {
                Some(records) => tree.extend(subtree_records(&records, &source.wanted)),
                None if source.pointed => {
                    tracing::warn!(
                        root = %id, from = %source.from,
                        "adopted sub-agents' log is unreadable; not resuming them"
                    );
                    let gone = closure(&own, source.wanted);
                    own.retain(|r| !gone.contains(&r.session));
                }
                None => {}
            }
        }
        self.visiting.remove(id);
        tree.extend(
            own.into_iter()
                .filter(|r| !matches!(r.payload, LogPayload::Adopted { .. })),
        );
        Some(tree)
    }

    /// Where `own` adopts from, one entry per file: its pointers, then — for
    /// the resumed root only — its immediate predecessor's tree, for sessions
    /// whose last move names a session of `own`. That one hop is enough: a
    /// pointer is lost only if the process dies between the move and the
    /// successor's start, so only the newest compaction can be missing one
    /// (a lost `ParentChanged` leaves a `Gap`, which refuses resume anyway).
    /// Older moves were pointed at before the next compaction, so resume
    /// cost follows the moved sub-trees, not the length of the chain.
    fn sources(&mut self, id: &SessionId, own: &[LogRecord], resumed: bool) -> Vec<Source> {
        let mut out: Vec<Source> = Vec::new();
        for r in own {
            if let LogPayload::Adopted { from } = &r.payload {
                add_source(&mut out, id, from, HashSet::from([r.session.clone()]), true);
            }
        }
        // The predecessor's *tree*, pointers resolved: a child moved more
        // than once logs its moves in its own spawn-time root, which the
        // predecessor reaches only through its pointer.
        if let Some(file) = resumed.then(|| predecessor_of(id, own)).flatten() {
            if let Some(records) = self.tree(&file, false) {
                let lost: HashSet<SessionId> = moved_into(&records, &sessions(own))
                    .into_iter()
                    .filter(|m| !out.iter().any(|s| s.wanted.contains(m)))
                    .collect();
                add_source(&mut out, id, &file, lost, false);
            }
        }
        out
    }
}

struct Source {
    from: SessionId,
    wanted: HashSet<SessionId>,
    /// Named by a pointer, not just found on the predecessor chain: only then
    /// is a missing file a lost sub-tree.
    pointed: bool,
}

/// Merge `wanted` from `from` into `out`, one entry per file.
fn add_source(
    out: &mut Vec<Source>,
    id: &SessionId,
    from: &SessionId,
    wanted: HashSet<SessionId>,
    pointed: bool,
) {
    if from == id || wanted.is_empty() {
        return;
    }
    match out.iter_mut().find(|s| &s.from == from) {
        Some(s) => {
            s.wanted.extend(wanted);
            s.pointed |= pointed;
        }
        None => out.push(Source {
            from: from.clone(),
            wanted,
            pointed,
        }),
    }
}

/// The sessions with records of their own in `records` (pointer rows aside).
fn sessions(records: &[LogRecord]) -> HashSet<SessionId> {
    records
        .iter()
        .filter(|r| !matches!(r.payload, LogPayload::Adopted { .. }))
        .map(|r| r.session.clone())
        .collect()
}

/// Root `id`'s own predecessor — a root successor's source, whose id is its
/// file's name.
fn predecessor_of(id: &SessionId, records: &[LogRecord]) -> Option<SessionId> {
    records.iter().find_map(|r| match &r.payload {
        LogPayload::Out(OutEvent::SessionStarted {
            session,
            predecessor: Some(p),
            ..
        }) if session == id => Some(p.clone()),
        _ => None,
    })
}

/// Sessions in `records` whose last `ParentChanged` names one of `members`.
fn moved_into(records: &[LogRecord], members: &HashSet<SessionId>) -> HashSet<SessionId> {
    let mut last: HashMap<&SessionId, &SessionId> = HashMap::new();
    for r in records {
        if let LogPayload::Out(OutEvent::ParentChanged { session, parent }) = &r.payload {
            last.insert(session, parent);
        }
    }
    last.into_iter()
        .filter(|(_, parent)| members.contains(*parent))
        .map(|(session, _)| session.clone())
        .collect()
}

/// `seeds` plus every session the records place under one of them, at any
/// depth — spawned (`SessionStarted.parent`) or moved (`ParentChanged`).
fn closure(records: &[LogRecord], seeds: HashSet<SessionId>) -> HashSet<SessionId> {
    let mut set = seeds;
    loop {
        let before = set.len();
        for r in records {
            let child = match &r.payload {
                LogPayload::Out(OutEvent::SessionStarted {
                    session,
                    parent: Some(p),
                    ..
                })
                | LogPayload::Out(OutEvent::ParentChanged {
                    session, parent: p, ..
                }) if set.contains(p) => session,
                _ => continue,
            };
            set.insert(child.clone());
        }
        if set.len() == before {
            return set;
        }
    }
}

/// The records of `wanted`'s sub-trees, in order. A gap tombstone inside
/// their span is kept: it cannot be attributed, so it must still make resume
/// refuse.
fn subtree_records(records: &[LogRecord], wanted: &HashSet<SessionId>) -> Vec<LogRecord> {
    let set = closure(records, wanted.clone());
    let keep = |r: &LogRecord| {
        set.contains(&r.session) && !matches!(r.payload, LogPayload::Adopted { .. })
    };
    let (Some(first), Some(last)) = (
        records.iter().position(keep),
        records.iter().rposition(keep),
    ) else {
        return Vec::new();
    };
    records[first..=last]
        .iter()
        .filter(|r| keep(r) || matches!(r.payload, LogPayload::Gap { .. }))
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
