//! Rolling tracked changes back (ADR-0212): place each change in a user turn,
//! select those after the target turn, and restore each touched file to its
//! state before the first selected change — unless it moved on since.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use entanglement_core::{FileRevertReport, RevertSkip, SessionId};

use super::load::{Live, Loaded};
use super::manifest::{self, Line, MANIFEST};
use super::restore::restore;
use super::{lock, FileBackups};
use crate::session_store::first_prompt_snippet;

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

impl FileBackups {
    /// The session's turns that still have unreverted tracked edits, oldest
    /// first.
    pub fn turns(&self, session: &SessionId) -> Result<Vec<TurnEdits>> {
        let loaded = self.load(&self.read_logs(session)?)?;
        let mut files: HashMap<u32, HashSet<&Path>> = HashMap::new();
        for l in &loaded.live {
            files.entry(l.turn).or_default().insert(&l.change.path);
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
    /// Refused (an `Err`) while any session of the sub-tree is still working,
    /// since its writes would race the restore.
    pub fn revert(&self, session: &SessionId, to_turn: Option<u32>) -> Result<FileRevertReport> {
        // The logs are parsed before the lock every tracked write takes, so a
        // large tree's revert never stalls them. A change recorded after this
        // read is still read under the lock, and the session making it is
        // busy, which refuses the revert anyway.
        let logs = self.read_logs(session)?;
        let _guard = lock(&self.inner.manifests);
        let loaded = self.load(&logs)?;
        let refuse = |busy: &SessionId| {
            anyhow::anyhow!(
                "refused: session `{}` in this session's tree is still working (a turn or \
                 background job), or its state was missed — revert once it is idle and has \
                 reported its status",
                busy.0
            )
        };
        if let Some(busy) = loaded.subtree.iter().find(|s| self.is_busy(s, session)) {
            return Err(refuse(busy));
        }
        // Backups recorded before `undo.enabled: false` still revert; only an
        // empty revert says why, so "nothing to undo" isn't mistaken for
        // "no edits were made".
        let disabled = || {
            anyhow::anyhow!(
                "undo disabled: file backups are off (`undo.enabled: false` in config.yml) \
                 and no earlier tracked edits are left to revert"
            )
        };
        let Some(target) = to_turn.or_else(|| undo_target(&loaded)) else {
            if !self.limits().enabled {
                return Err(disabled());
            }
            return Ok(FileRevertReport {
                to_turn: loaded.prompts.len() as u32,
                ..Default::default()
            });
        };

        // Group the selected changes per file, oldest first.
        let mut order: Vec<&Path> = Vec::new();
        let mut groups: HashMap<&Path, Vec<&Live>> = HashMap::new();
        for l in loaded.live.iter().filter(|l| l.turn > target) {
            let group = groups.entry(&l.change.path).or_default();
            if group.is_empty() {
                order.push(&l.change.path);
            }
            group.push(l);
        }
        if order.is_empty() && !self.limits().enabled {
            return Err(disabled());
        }
        // The busy set comes from the logs, which can lag a move: a moved
        // child whose `ParentChanged` is not logged yet is missing from it,
        // though its recorded changes are selected. Their writers must be
        // idle too.
        let writers: HashSet<&SessionId> = groups
            .values()
            .flatten()
            .map(|l| &l.change.session)
            .collect();
        if let Some(busy) = writers.into_iter().find(|s| self.is_busy(s, session)) {
            return Err(refuse(busy));
        }

        let mut report = FileRevertReport {
            to_turn: target,
            ..Default::default()
        };
        // Per backup dir: a change's id is only unique within its manifest.
        let mut marks: HashMap<usize, (Vec<u64>, Vec<u64>)> = HashMap::new();
        for path in order {
            let group = &groups[path];
            let (first, last) = (group[0], group[group.len() - 1]);
            // The prior bytes are a blob beside the first change's manifest.
            let outcome = restore(
                &loaded.dirs[first.dir],
                path,
                &first.change.before,
                last.change.after.as_deref(),
                first.change.mode,
            );
            let consumed = match outcome {
                Ok(()) => {
                    report.restored.push(last.change.display.clone());
                    true
                }
                Err(skip) => {
                    report.skipped.push(RevertSkip {
                        path: last.change.display.clone(),
                        reason: skip.reason,
                    });
                    skip.consumed
                }
            };
            for l in group {
                let (done, attempted) = marks.entry(l.dir).or_default();
                if consumed { done } else { attempted }.push(l.change.id);
            }
        }
        for (dir, (ids, attempted)) in marks {
            // The files are already restored: a lost marker must not turn
            // that into a failure. The next revert finds them back at
            // `before` and counts them restored again.
            let line = Line::Reverted { ids, attempted };
            if let Err(e) = manifest::append(&loaded.dirs[dir].join(MANIFEST), &line) {
                tracing::warn!("revert succeeded but its marker was not recorded: {e:#}");
            }
        }
        Ok(report)
    }
}

/// An undo's target: just below the latest turn with changes no revert has
/// attempted yet, so a file left alone for a conflict never pins the undo
/// to the same turn; with only attempted ones left, it retries the latest.
fn undo_target(loaded: &Loaded) -> Option<u32> {
    let latest = |fresh: bool| {
        loaded
            .live
            .iter()
            .filter(|l| !fresh || !l.attempted)
            .map(|l| l.turn)
            .max()
    };
    latest(true)
        .or_else(|| latest(false))
        .map(|t| t.saturating_sub(1))
}
