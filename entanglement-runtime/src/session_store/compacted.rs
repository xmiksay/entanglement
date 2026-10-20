//! Whether a logged root session was compacted away (ADR-0243): a later root
//! log's own `SessionStarted` names it as its predecessor. A compacted-away
//! session is not resumable: its lineage goes on in its successor, and
//! resuming it would branch the lineage (fork it to branch). Core refuses
//! one it already knows; a fresh process learns it only from the logs.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use entanglement_core::{OutEvent, SessionId};

use super::{LogPayload, LogRecord, SessionStore};

impl SessionStore {
    /// The last link of `id`'s compaction chain as this store's logs record
    /// it, or `None` when no root log names `id` as its predecessor.
    pub fn compacted_into(&self, id: &SessionId) -> Result<Option<SessionId>> {
        let edges = successor_edges(&self.dir()?)?;
        let Some(mut end) = edges.get(id).cloned() else {
            return Ok(None);
        };
        // Bounded by the map, so a corrupt cycle can't spin.
        for _ in 0..edges.len() {
            match edges.get(&end) {
                Some(next) if next != id => end = next.clone(),
                _ => break,
            }
        }
        Ok(Some(end))
    }
}

/// predecessor → successor, from each root log's own `SessionStarted`.
fn successor_edges(dir: &Path) -> Result<HashMap<SessionId, SessionId>> {
    let mut edges = HashMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(edges),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let id = SessionId::new(stem);
        if let Some(predecessor) = own_predecessor(&path, &id) {
            edges.insert(predecessor, id);
        }
    }
    Ok(edges)
}

/// Read `path` only up to its first `SessionStarted`, a few lines in: a
/// root's own start is its file's first, so a later one (a child's) means the
/// root's is missing and nothing is named. An unreadable file names no
/// predecessor either: listing skips it the same way.
fn own_predecessor(path: &Path, id: &SessionId) -> Option<SessionId> {
    let file = File::open(path).ok()?;
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if !line.contains("session_started") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<LogRecord>(&line) else {
            continue;
        };
        if let LogPayload::Out(OutEvent::SessionStarted {
            session,
            predecessor,
            ..
        }) = record.payload
        {
            return (session == *id).then_some(predecessor).flatten();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: &SessionId, predecessor: Option<&SessionId>) -> LogRecord {
        LogRecord::new(
            id.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: id.clone(),
                parent: None,
                predecessor: predecessor.cloned(),
                agent: "general".to_string(),
                model: None,
                root: true,
                ts: 0,
                user: None,
            }),
        )
    }

    #[test]
    fn a_compacted_away_root_names_the_end_of_its_chain() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::at(temp.path());
        let [p, s1, s2, other] = ["p", "s1", "s2", "other"].map(SessionId::new);
        store.append(&p, &started(&p, None)).expect("append");
        store.append(&s1, &started(&s1, Some(&p))).expect("append");
        store.append(&s2, &started(&s2, Some(&s1))).expect("append");
        store
            .append(&other, &started(&other, None))
            .expect("append");
        assert_eq!(store.compacted_into(&p).unwrap(), Some(s2.clone()));
        assert_eq!(store.compacted_into(&s1).unwrap(), Some(s2.clone()));
        assert_eq!(store.compacted_into(&s2).unwrap(), None, "the live end");
        assert_eq!(store.compacted_into(&other).unwrap(), None);
    }

    #[test]
    fn resuming_a_compacted_away_root_is_refused() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::at(temp.path());
        let [p, s] = ["p", "s"].map(SessionId::new);
        store.append(&p, &started(&p, None)).expect("append");
        store.append(&s, &started(&s, Some(&p))).expect("append");
        let err = store.load_for_resume(&p, |_| true).unwrap_err();
        assert!(format!("{err:#}").contains("compacted into `s`"), "{err:#}");
        assert!(store.load_for_resume(&s, |_| true).is_ok());
    }
}
