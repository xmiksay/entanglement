//! Rolling tracked changes back (ADR-0212): place each change in a user turn,
//! select those after the target turn, and restore each touched file to its
//! state before the first selected change — unless it moved on since.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use entanglement_core::{content_text, FileRevertReport, InMsg, OutEvent, RevertSkip, SessionId};

use super::manifest::{self, Change, Line, Prior, BLOBS, MANIFEST};
use super::{lock, sha256_hex, FileBackups};
use crate::session_store::{first_prompt_snippet, LogPayload, LogRecord};

/// One user turn that still has unreverted tracked edits — a `/rewind`
/// picker row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEdits {
    /// 1-based user-prompt index within the session.
    pub turn: u32,
    /// Distinct files the turn's unreverted edits touched.
    pub files: usize,
    /// The turn's prompt, shortened for a listing.
    pub prompt: String,
}

/// What a revert works from: the backup dir, the session's prompt texts in
/// order, and its sub-tree's unreverted changes, each with its turn.
struct Loaded {
    dir: PathBuf,
    prompts: Vec<String>,
    live: Vec<(Change, u32)>,
}

/// One of the session's prompts in the log: record index, timestamp, text.
struct Prompt {
    pos: usize,
    ts: u64,
    text: String,
}

/// The 1-based turn a change belongs to: how many of the session's prompts
/// precede it (0 = before the first prompt). Placed by the log position of
/// the `ToolExec` that made it — the tap logs a prompt ahead of everything it
/// causes, so position is exact. A change whose call is not in the log (a
/// lagging or gapped tap) falls back to comparing record timestamps.
fn turn_of(change: &Change, prompts: &[Prompt], exec_pos: &HashMap<&str, usize>) -> u32 {
    // A `rhai` binding runs under `<outer request id>:rhai:<tool>`; the outer
    // `rhai` call is the one in the log.
    let call = change.request_id.split(":rhai:").next().unwrap_or_default();
    let count = match exec_pos.get(call) {
        Some(&pos) => prompts.iter().filter(|p| p.pos < pos).count(),
        None => prompts.iter().filter(|p| p.ts <= change.ts).count(),
    };
    count as u32
}

impl FileBackups {
    fn load(&self, session: &SessionId) -> Result<Loaded> {
        let root = self.root_of(session);
        let records = self
            .inner
            .store
            .read(&root)
            .with_context(|| format!("reading the session log of `{}`", root.0))?;
        let subtree = subtree(&records, session);
        let mut prompts = Vec::new();
        let mut exec_pos: HashMap<&str, usize> = HashMap::new();
        for (pos, r) in records.iter().enumerate() {
            match &r.payload {
                LogPayload::In(InMsg::Prompt {
                    session: s,
                    content,
                }) if s == session => prompts.push(Prompt {
                    pos,
                    ts: r.ts,
                    text: content_text(content),
                }),
                LogPayload::Out(OutEvent::ToolExec { request_id, .. }) => {
                    exec_pos.entry(request_id.as_str()).or_insert(pos);
                }
                _ => {}
            }
        }
        let dir = self.inner.store.backups_dir(&root)?;
        let manifest = manifest::read(&dir.join(MANIFEST))?;
        let live = manifest
            .changes
            .into_iter()
            .filter(|c| !manifest.reverted.contains(&c.id) && subtree.contains(&c.session))
            .map(|c| {
                let turn = turn_of(&c, &prompts, &exec_pos);
                (c, turn)
            })
            .collect();
        Ok(Loaded {
            dir,
            prompts: prompts.into_iter().map(|p| p.text).collect(),
            live,
        })
    }

    /// The session's turns that still have unreverted tracked edits, oldest
    /// first.
    pub fn turns(&self, session: &SessionId) -> Result<Vec<TurnEdits>> {
        let loaded = self.load(session)?;
        let mut files: HashMap<u32, HashSet<&Path>> = HashMap::new();
        for (c, turn) in &loaded.live {
            files.entry(*turn).or_default().insert(&c.path);
        }
        let mut turns: Vec<TurnEdits> = files
            .into_iter()
            .map(|(turn, paths)| TurnEdits {
                turn,
                files: paths.len(),
                prompt: turn
                    .checked_sub(1)
                    .and_then(|i| loaded.prompts.get(i as usize))
                    .map(|text| first_prompt_snippet(text))
                    .unwrap_or_default(),
            })
            .collect();
        turns.sort_by_key(|t| t.turn);
        Ok(turns)
    }

    /// Revert `session`'s (and its sub-tree's) tracked changes made after
    /// turn `to_turn`; `None` reverts the latest turn that still has any.
    pub fn revert(&self, session: &SessionId, to_turn: Option<u32>) -> Result<FileRevertReport> {
        let _guard = lock(&self.inner.lock);
        let loaded = self.load(session)?;
        let target = match to_turn {
            Some(n) => n,
            None => match loaded.live.iter().map(|(_, turn)| *turn).max() {
                Some(latest) => latest.saturating_sub(1),
                None => {
                    return Ok(FileRevertReport {
                        to_turn: loaded.prompts.len() as u32,
                        ..Default::default()
                    })
                }
            },
        };

        // Group the selected changes per file, in record order.
        let mut order: Vec<&Path> = Vec::new();
        let mut groups: HashMap<&Path, Vec<&Change>> = HashMap::new();
        for (c, _) in loaded.live.iter().filter(|(_, turn)| *turn > target) {
            let group = groups.entry(&c.path).or_default();
            if group.is_empty() {
                order.push(&c.path);
            }
            group.push(c);
        }

        let mut report = FileRevertReport {
            to_turn: target,
            ..Default::default()
        };
        let mut reverted_ids = Vec::new();
        for path in order {
            let group = &groups[path];
            let (first, last) = (group[0], group[group.len() - 1]);
            match restore(&loaded.dir, path, &first.before, last.after.as_deref()) {
                Ok(()) => {
                    report.restored.push(last.display.clone());
                    reverted_ids.extend(group.iter().map(|c| c.id));
                }
                Err(reason) => report.skipped.push(RevertSkip {
                    path: last.display.clone(),
                    reason,
                }),
            }
        }
        if !reverted_ids.is_empty() {
            manifest::append(
                &loaded.dir.join(MANIFEST),
                &Line::Reverted { ids: reverted_ids },
            )?;
        }
        Ok(report)
    }
}

/// Put `path` back to `before`, provided it still holds what the agent left
/// (`expected` hash, `None` = no file). `Err` is the user-facing reason.
fn restore(
    dir: &Path,
    path: &Path,
    before: &Prior,
    expected: Option<&str>,
) -> std::result::Result<(), String> {
    let current = match std::fs::read(path) {
        Ok(bytes) => Some(sha256_hex(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("could not read it: {e}")),
    };
    if current.as_deref() != expected {
        return Err("changed since the agent's edit — left as is".to_string());
    }
    match before {
        Prior::Skipped { reason } => Err(format!("no backup ({reason})")),
        Prior::Absent => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("could not delete it: {e}")),
        },
        Prior::Stored { hash, .. } => {
            let bytes = std::fs::read(dir.join(BLOBS).join(hash))
                .map_err(|e| format!("backup missing: {e}"))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("could not recreate its directory: {e}"))?;
            }
            std::fs::write(path, bytes).map_err(|e| format!("could not write it: {e}"))
        }
    }
}

/// `session` plus every session spawned beneath it, from the log's
/// `SessionStarted` lineage.
fn subtree(records: &[LogRecord], session: &SessionId) -> HashSet<SessionId> {
    let mut children: HashMap<&SessionId, Vec<&SessionId>> = HashMap::new();
    for r in records {
        if let LogPayload::Out(OutEvent::SessionStarted {
            session: child,
            parent: Some(parent),
            ..
        }) = &r.payload
        {
            children.entry(parent).or_default().push(child);
        }
    }
    let mut out = HashSet::from([session.clone()]);
    let mut queue = VecDeque::from([session]);
    while let Some(s) = queue.pop_front() {
        for child in children.get(s).into_iter().flatten() {
            if out.insert((*child).clone()) {
                queue.push_back(child);
            }
        }
    }
    out
}
