//! `Resume`: rebuild a session (and its still-live spawn sub-tree) from the
//! embedder's replayed log.

use tokio::sync::mpsc;

use super::Supervisor;
use crate::holly::routing::{emit_supervisor_error, resume_meta};
use crate::holly::SESSION_CMD_CAPACITY;
use crate::protocol::{InMsg, OutEvent, SessionId};
use crate::session::{session_loop, Records, Session, SessionCmd, SessionStart};

impl Supervisor {
    /// `Resume`: refuse a closed, live or compacted-away id, else replay
    /// `session_id` and cascade over its sub-tree.
    pub(super) async fn resume(
        &mut self,
        session_id: &SessionId,
        records: &[(Option<InMsg>, OutEvent)],
    ) {
        // A retired id is single-use; refuse rather than resurrect (ADR-0028).
        if self.closed.contains(session_id) {
            emit_supervisor_error(
                &self.events,
                &self.seqs,
                session_id,
                "cannot resume a closed session id (ids are single-use)",
            );
            return;
        }
        // Resuming a live id would overwrite its sender and orphan the running
        // task (it sees its channel close mid-turn). Refuse, like `Spawn`.
        if self.sessions.contains_key(session_id) {
            emit_supervisor_error(
                &self.events,
                &self.seqs,
                session_id,
                "cannot resume an already-live session id",
            );
            return;
        }
        // A compacted-away session is not resumable (ADR-0243): its lineage
        // goes on in its successor, and a second line would branch it.
        // `ForkSession` is the way to branch.
        if let Some(end) = self.session_meta.successor_of(session_id) {
            emit_supervisor_error(
                &self.events,
                &self.seqs,
                session_id,
                &format!(
                    "session `{}` compacted into `{end}`; resume `{end}` (fork `{end}` to \
                     branch)",
                    session_id.0,
                    end = end.0
                ),
            );
            return;
        }
        self.learn_lineages(records);
        let Some(children) = self.spawn_resumed(session_id, records) else {
            // A failure already emitted an `Error`; leave the id unclaimed
            // rather than half-registering it (issue #105).
            return;
        };
        // Cascade (#415): a root's log carries its whole spawn sub-tree
        // (persistence folds every spawned child into the root file), so a
        // sub-agent still "live" as of where recording stopped — a
        // `SessionStarted` with no matching `SessionEnded`/`SessionHibernated`
        // — is re-materialized too, the same way `CloseSession`/
        // `HibernateSession` cascade over the sub-tree on teardown. Without
        // this, a resumed parent's `children` mirror lists ids with no task
        // behind them, and touching one hits the lazy-`Prompt` path's
        // known-child refusal (issue #639) instead of actually resuming.
        let mut queue: Vec<SessionId> = children;
        let mut cursor = 0;
        while cursor < queue.len() {
            let child = queue[cursor].clone();
            cursor += 1;
            if self.closed.contains(&child) || self.sessions.contains_key(&child) {
                continue;
            }
            // A child still unwinding from a hibernate stays hibernated: unless
            // its task panicked, its `SessionHibernated` is still to come, so
            // these records only lack it, and the complete log (or a fresh
            // process) would not revive it either. A panicked task writes no
            // `SessionHibernated`, so its complete log would revive it; that
            // edge is accepted (the watcher logs the panic). An explicit
            // `Resume` of it starts a task that waits for its old task's exit
            // (#892, ADR-0244 §4).
            if self.retiring.contains_key(&child) {
                continue;
            }
            if let Some(grandchildren) = self.spawn_resumed(&child, records) {
                queue.extend(grandchildren);
            }
        }
    }

    /// Re-learn the compaction lineages `records` hold, so a fresh process
    /// can still resolve a retired id to its successor and a successor back
    /// to the handle its parent knew (ADR-0234), and an ancestor walk maps a
    /// compacted ancestor to its successor (ADR-0246).
    pub(super) fn learn_lineages(&self, records: &[(Option<InMsg>, OutEvent)]) {
        for (_, ev) in records {
            if let OutEvent::SessionStarted {
                session,
                predecessor: Some(source),
                ..
            } = ev
            {
                self.session_meta.record_successor(source, session);
            }
        }
    }

    /// Replay `target` from a shared log and register it exactly like a live
    /// `Spawn`/`Resume` would — `session_meta`, `parent_links` (when it has a
    /// parent), a spawned `session_loop` task, and the `sessions` map entry.
    /// Returns `target`'s live `children` (reconstructed by
    /// [`Session::replay`]) so a caller can keep cascading over the sub-tree
    /// (#415, see [`resume`][Self::resume]); `None` on a replay failure, after
    /// emitting a supervisor `Error` for `target`.
    ///
    /// `records` is the whole root log (children fold into their root's file,
    /// [ADR-0020](../../docs/adr/0020-event-sourced-session-persistence.md)), and
    /// [`Session::replay`] scopes its fold to `target`'s own records regardless of
    /// which session in the tree `target` names (#275).
    fn spawn_resumed(
        &mut self,
        target: &SessionId,
        records: &[(Option<InMsg>, OutEvent)],
    ) -> Option<Vec<SessionId>> {
        // Replay *before* registering the session. A failed replay used to still
        // insert the sender while its task returned early, leaving a dead id that
        // showed in `ListSessions` and silently swallowed every routed `Prompt`
        // (issue #105). Register only on success; on failure surface an `Error`
        // and leave the id unclaimed.
        let mut initial_session = match Session::replay(records, &self.cfg, target) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to replay session {}: {}", target, e);
                emit_supervisor_error(
                    &self.events,
                    &self.seqs,
                    target,
                    &format!("failed to resume session: {e}"),
                );
                return None;
            }
        };
        let parent = initial_session.parent.clone();
        let mut meta = resume_meta(target, records);
        // The replayed parent folds `ParentChanged` (ADR-0234); the
        // `SessionStarted` `resume_meta` reads names the spawn-time one.
        meta.parent = parent.clone();
        self.session_meta.insert(meta);
        if let Some(p) = parent.as_ref() {
            self.parent_links.insert(target.clone(), Some(p.clone()));
        }
        // The mode it resumes in: its replayed one, unless its nearest
        // ancestor's current mode is stricter (ADR-0246) — an ancestor's
        // `SetMode` while it was hibernated reaches it here. The task applies
        // it at start, and its start-up `ModeChanged` logs the switch.
        let mode = self.resumed_mode(target, &initial_session.mode, parent.clone(), records);
        self.modes.insert(target.clone(), mode.clone());
        // While its old task still tears down, the log can lag the workdir
        // that task last ran in; the cache, written by every validated
        // `SetWorkdir`, is the newer word (ADR-0246).
        if self.retiring.contains_key(target) {
            if let Some(dir) = self.workdirs.get(target) {
                initial_session.workdir = Some(dir.clone());
            }
        }
        // Same for the workdir cache (#764), after re-validating the
        // replayed workdir: a resumed parent's new children inherit it.
        self.revalidate_resumed_workdir(target, &mut initial_session.workdir);
        let children = initial_session.children.clone();
        let (stx, srx) = mpsc::channel::<SessionCmd>(SESSION_CMD_CAPACITY);
        let ev = self.events.clone();
        let cfg2 = self.cfg.clone();
        let sid = target.clone();
        let profile = initial_session.agent.clone();
        let seqs2 = self.seqs.clone();
        let activity2 = self.activity.clone();
        let forks = self.fork_tx.clone();
        // Waits inside the task for an old task still tearing down (ADR-0244),
        // then re-folds these records with that task's teardown tail
        // (ADR-0246).
        let prior = self.retiring.remove(target);
        let kept: Option<Records> = prior.as_ref().map(|_| records.into());
        let task = tokio::spawn(async move {
            session_loop(
                sid,
                srx,
                ev,
                cfg2,
                SessionStart {
                    agent: profile,
                    initial_session: Some(initial_session),
                    parent,
                    // Resume reconstructs `predecessor`/`user`/model from
                    // the log (replay); pass `None` for them so none is
                    // overwritten (session.rs's resumed-takes-precedence
                    // rule).
                    predecessor: None,
                    user: None,
                    initial_mode: mode,
                    spawn_model: None,
                    inherited: None,
                    initial_workdir: None,
                    prior,
                    records: kept,
                },
                seqs2,
                activity2,
                forks,
            )
            .await
        });
        self.sessions.insert(target.clone(), stx);
        self.tasks.insert(target.clone(), task);
        Some(children)
    }
}
