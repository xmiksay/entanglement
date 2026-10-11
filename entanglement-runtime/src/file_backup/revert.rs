//! Rolling tracked changes back (ADR-0212): place each change in a user turn,
//! select those after the target turn, and restore each touched file to its
//! state before the first selected change — unless it moved on since.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use entanglement_core::{content_text, FileRevertReport, InMsg, OutEvent, RevertSkip, SessionId};

use super::manifest::{self, Change, Line, MANIFEST};
use super::restore::restore;
use super::{lock, FileBackups};
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

/// What a revert works from: the backup dir, the session's sub-tree and
/// prompt texts, and the sub-tree's unreverted changes, each with its turn.
struct Loaded {
    dir: PathBuf,
    subtree: HashSet<SessionId>,
    prompts: Vec<String>,
    live: Vec<(Change, u32)>,
    attempted: HashSet<u64>,
}

/// One of the target session's prompts in the log: record index, timestamp.
struct Prompt {
    pos: usize,
    ts: u64,
}

/// Where each `ToolExec` sits in the log, by the two keys a change can carry.
#[derive(Default)]
struct ExecIndex<'a> {
    by_seq: HashMap<(&'a SessionId, u64), usize>,
    /// Every `(session, request_id)` occurrence as `(pos, ts)`, in log order —
    /// ids repeat across turns (Gemini's `name#ordinal`), so a lookup takes
    /// the latest one logged no later than the change.
    by_call: HashMap<(&'a SessionId, &'a str), Vec<(usize, u64)>>,
}

impl ExecIndex<'_> {
    /// The log position of the call that made `change`, if found.
    fn position(&self, change: &Change) -> Option<usize> {
        if let Some(&pos) = change
            .exec_seq
            .and_then(|seq| self.by_seq.get(&(&change.session, seq)))
        {
            return Some(pos);
        }
        // A `rhai` binding runs under `<outer request id>:rhai:<tool>`; the
        // outer `rhai` call is the one in the log.
        let call = change.request_id.split(":rhai:").next().unwrap_or_default();
        self.by_call
            .get(&(&change.session, call))?
            .iter()
            .rev()
            .find(|(_, ts)| *ts <= change.ts)
            .map(|(pos, _)| *pos)
    }
}

/// The 1-based turn a change belongs to: how many of the session's prompts
/// precede the call that made it (0 = before the first prompt). With no call
/// found in the log (a lagging or gapped tap), record timestamps decide.
fn turn_of(change: &Change, prompts: &[Prompt], index: &ExecIndex<'_>) -> u32 {
    let count = match index.position(change) {
        Some(pos) => prompts.iter().filter(|p| p.pos < pos).count(),
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
        let mut texts = Vec::new();
        let mut index = ExecIndex::default();
        for (pos, r) in records.iter().enumerate() {
            match &r.payload {
                LogPayload::In(InMsg::Prompt {
                    session: s,
                    content,
                }) if s == session => {
                    prompts.push(Prompt { pos, ts: r.ts });
                    texts.push(content_text(content));
                }
                LogPayload::Out(OutEvent::ToolExec {
                    session: s,
                    seq,
                    request_id,
                    ..
                }) => {
                    index.by_seq.entry((s, *seq)).or_insert(pos);
                    index
                        .by_call
                        .entry((s, request_id.as_str()))
                        .or_default()
                        .push((pos, r.ts));
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
                let turn = turn_of(&c, &prompts, &index);
                (c, turn)
            })
            .collect();
        Ok(Loaded {
            dir,
            subtree,
            prompts: texts,
            live,
            attempted: manifest.attempted,
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
    /// Refused (an `Err`) while any session of the sub-tree is still working,
    /// since its writes would race the restore.
    pub fn revert(&self, session: &SessionId, to_turn: Option<u32>) -> Result<FileRevertReport> {
        let _guard = lock(&self.inner.manifests);
        let loaded = self.load(session)?;
        if let Some(busy) = loaded.subtree.iter().find(|s| self.is_busy(s, session)) {
            anyhow::bail!(
                "refused: session `{}` in this session's tree is still working (a turn or \
                 background job), or its state was missed — revert once it is idle and has \
                 reported its status",
                busy.0
            );
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
        if order.is_empty() && !self.limits().enabled {
            return Err(disabled());
        }

        let mut report = FileRevertReport {
            to_turn: target,
            ..Default::default()
        };
        let (mut done, mut attempted) = (Vec::new(), Vec::new());
        for path in order {
            let group = &groups[path];
            let (first, last) = (group[0], group[group.len() - 1]);
            let ids = group.iter().map(|c| c.id);
            let outcome = restore(
                &loaded.dir,
                path,
                &first.before,
                last.after.as_deref(),
                first.mode,
            );
            match outcome {
                Ok(()) => {
                    report.restored.push(last.display.clone());
                    done.extend(ids);
                }
                Err(skip) => {
                    report.skipped.push(RevertSkip {
                        path: last.display.clone(),
                        reason: skip.reason,
                    });
                    if skip.consumed {
                        done.extend(ids);
                    } else {
                        attempted.extend(ids);
                    }
                }
            }
        }
        if !done.is_empty() || !attempted.is_empty() {
            let line = Line::Reverted {
                ids: done,
                attempted,
            };
            // The files are already restored: a lost marker must not turn
            // that into a failure. The next revert finds them back at
            // `before` and counts them restored again.
            if let Err(e) = manifest::append(&loaded.dir.join(MANIFEST), &line) {
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
            .filter(|(c, _)| !fresh || !loaded.attempted.contains(&c.id))
            .map(|(_, turn)| *turn)
            .max()
    };
    latest(true)
        .or_else(|| latest(false))
        .map(|t| t.saturating_sub(1))
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
