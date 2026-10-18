//! What a revert works from (ADR-0212): the target session's sub-tree, its
//! prompts, and the sub-tree's unreverted changes, each placed in a turn.
//!
//! A sub-agent a compaction moved under the successor (ADR-0234) keeps
//! logging, and backing up, under the root it started in. So the backups are
//! read across every file [`SessionStore::tree_files`] reaches through the
//! `Adopted` pointers, not just the target root's own (ADR-0236, #870).
//! Nothing is moved: each change stays in the manifest it was written to, and
//! its revert marker goes back there.
//!
//! Which changes are the target's: a change records the live tree it was
//! made in (`Change::ancestry`), so a revert of `T` takes those naming `T`.
//! A change with no ancestry (an older manifest, or a session the engine no
//! longer knew) goes by the pre-#870 rule: its session is in the target's
//! tree now, by each session's latest parent edge in the logs (ADR-0236 §8).
//!
//! Reading is split in two so a revert parses the logs before it takes the
//! manifest lock every tracked write needs: [`FileBackups::read_logs`]
//! (unlocked) and [`FileBackups::load`] (the manifests, under the lock).
//!
//! [`SessionStore::tree_files`]: crate::session_store::SessionStore::tree_files

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use entanglement_core::{content_text, InMsg, OutEvent, SessionId};

use super::lineage::{current_tree, merge_by};
use super::manifest::{self, Change, MANIFEST};
use super::FileBackups;
use crate::session_store::{LogPayload, LogRecord};

/// The logs of the target's tree, one entry per file, the root's own first.
pub(super) struct Logs {
    target: SessionId,
    files: Vec<(SessionId, Vec<LogRecord>)>,
}

/// The loaded state of one revert or listing.
pub(super) struct Loaded {
    /// The backup directory of each file read; [`Live::dir`] indexes it.
    pub dirs: Vec<PathBuf>,
    /// The target and every session under it now — the busy check's set.
    pub subtree: HashSet<SessionId>,
    /// The target's prompt texts, oldest first: turn `n` is `prompts[n - 1]`.
    pub prompts: Vec<String>,
    /// Each manifest in its own record order, merged across manifests by
    /// wall clock.
    pub live: Vec<Live>,
}

/// One unreverted change of the sub-tree.
pub(super) struct Live {
    pub change: Change,
    pub turn: u32,
    /// Index into [`Loaded::dirs`]: where its blob and its marker live.
    pub dir: usize,
    /// A revert already selected it but left the file alone (a conflict).
    pub attempted: bool,
}

/// One of the target's prompts: the file it is logged in, its record index
/// there, its timestamp.
struct Prompt {
    file: usize,
    pos: usize,
    ts: u64,
}

/// Where each `ToolExec` sits in one file, by the two keys a change can carry.
#[derive(Default)]
struct ExecIndex<'a> {
    by_seq: HashMap<(&'a SessionId, u64), usize>,
    /// Every `(session, request_id)` occurrence as `(pos, ts)`, in log order —
    /// ids repeat across turns (Gemini's `name#ordinal`), so a lookup takes
    /// the latest one logged no later than the change.
    by_call: HashMap<(&'a SessionId, &'a str), Vec<(usize, u64)>>,
}

impl<'a> ExecIndex<'a> {
    fn of(records: &'a [LogRecord]) -> Self {
        let mut index = Self::default();
        for (pos, r) in records.iter().enumerate() {
            if let LogPayload::Out(OutEvent::ToolExec {
                session,
                seq,
                request_id,
                ..
            }) = &r.payload
            {
                index.by_seq.entry((session, *seq)).or_insert(pos);
                index
                    .by_call
                    .entry((session, request_id.as_str()))
                    .or_default()
                    .push((pos, r.ts));
            }
        }
        index
    }

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

/// The 1-based turn a change recorded beside `file` belongs to: how many of
/// the target's prompts precede it (0 = before the first prompt). A prompt in
/// the same file is ordered by record position against the change's call, as
/// the tap logs both in broadcast order. A prompt in another file — the moved
/// sub-agent's case, whose calls log in its spawn-time root while the
/// successor's prompts log in its own — shares no positions with it, so the
/// wall clock of the write decides. So does a change whose call is not in the
/// log (a lagging or gapped tap).
fn turn_of(change: &Change, file: usize, call: Option<usize>, prompts: &[Prompt]) -> u32 {
    let count = prompts
        .iter()
        .filter(|p| match call {
            Some(pos) if p.file == file => p.pos < pos,
            _ => p.ts <= change.ts,
        })
        .count();
    count as u32
}

impl FileBackups {
    /// Every log file of `session`'s root tree — no lock held.
    pub(super) fn read_logs(&self, session: &SessionId) -> Result<Logs> {
        let root = self.root_of(session);
        let files = self
            .inner
            .store
            .tree_files(&root)
            .with_context(|| format!("reading the session log of `{}`", root.0))?;
        Ok(Logs {
            target: session.clone(),
            files,
        })
    }

    /// The target's changes from every file's manifest, placed by `logs`.
    pub(super) fn load(&self, logs: &Logs) -> Result<Loaded> {
        let target = &logs.target;
        let subtree = current_tree(&logs.files, target);

        let mut prompts = Vec::new();
        let mut texts = Vec::new();
        for (file, (_, records)) in logs.files.iter().enumerate() {
            let mut file_texts = Vec::new();
            for (pos, r) in records.iter().enumerate() {
                if let LogPayload::In(InMsg::Prompt {
                    session: s,
                    content,
                }) = &r.payload
                {
                    if s == target {
                        prompts.push(Prompt {
                            file,
                            pos,
                            ts: r.ts,
                        });
                        file_texts.push((r.ts, content_text(content)));
                    }
                }
            }
            texts.push(file_texts);
        }
        // A target's prompts sit in one file unless it is itself a moved
        // sub-agent that logged on after a resume; across files only the wall
        // clock orders them.
        let texts = merge_by(texts, |(ts, _)| *ts);

        let mut dirs = Vec::new();
        let mut per_dir = Vec::new();
        for (file, (id, records)) in logs.files.iter().enumerate() {
            let dir = self.inner.store.backups_dir(id)?;
            let path = dir.join(MANIFEST);
            dirs.push(dir);
            let manifest = match manifest::read(&path) {
                Ok(m) => m,
                // An adopted file's backups are a bonus: losing them must not
                // block reverting the target's own.
                Err(e) if file != 0 => {
                    tracing::warn!("skipping unreadable adopted backups: {e:#}");
                    per_dir.push(Vec::new());
                    continue;
                }
                Err(e) => return Err(e),
            };
            let index = ExecIndex::of(records);
            let mut live = Vec::new();
            for change in manifest.changes {
                if manifest.reverted.contains(&change.id) {
                    continue;
                }
                // A recorded change belongs to the trees it was made in — a
                // moved sub-agent's write from before the move is not the
                // successor's. One without (legacy) goes by the tree now.
                let recorded = !change.ancestry.is_empty();
                let in_tree = if recorded {
                    change.ancestry.contains(target)
                } else {
                    subtree.contains(&change.session)
                };
                if !in_tree {
                    continue;
                }
                let mut turn = turn_of(&change, file, index.position(&change), &prompts);
                // A change made inside the target's tree is the target's work,
                // so it is in turn 1 at least, even when the wall clock says
                // it came before the target's first prompt: a compaction
                // successor's seed prompt is logged only once its own start
                // arrives, after the move.
                if recorded {
                    turn = turn.max(1);
                }
                live.push(Live {
                    attempted: manifest.attempted.contains(&change.id),
                    change,
                    turn,
                    dir: file,
                });
            }
            per_dir.push(live);
        }
        Ok(Loaded {
            dirs,
            subtree,
            prompts: texts.into_iter().map(|(_, text)| text).collect(),
            live: merge_by(per_dir, |l| l.change.ts),
        })
    }
}
