//! `Resume`: rebuild a session (and its still-live spawn sub-tree) from the
//! embedder's replayed log.

use tokio::sync::mpsc;

use super::Supervisor;
use crate::holly::routing::{emit_supervisor_error, resume_meta};
use crate::holly::{DEFAULT_MODE, SESSION_CMD_CAPACITY};
use crate::protocol::{InMsg, OutEvent, SessionId};
use crate::session::{session_loop, Session, SessionCmd, SessionStart};

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
        // Re-learn the compaction lineages the log records, so a fresh
        // process can still resolve a retired id to its successor and a
        // successor back to the handle its parent knew (ADR-0234).
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
            if let Some(grandchildren) = self.spawn_resumed(&child, records) {
                queue.extend(grandchildren);
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
        // Re-derive the mode cache from the replayed truth rather than trusting
        // whatever it held before this session hibernated (ADR-0207 §6): correct
        // either way (hibernation itself never changes mode), but this is what
        // keeps the cache right the first time a *newly* resumed id (never seen
        // by this process before, e.g. a fresh `skutter` instance) spawns a
        // child of its own.
        self.modes
            .insert(target.clone(), initial_session.mode.clone());
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
        tokio::spawn(async move {
            session_loop(
                sid,
                srx,
                ev,
                cfg2,
                SessionStart {
                    agent: profile,
                    initial_session: Some(initial_session),
                    parent,
                    // Resume reconstructs `predecessor`/`user`/`mode`/model
                    // from the log (replay); pass `None`/`DEFAULT_MODE` for
                    // them so none is overwritten (session.rs's
                    // resumed-takes-precedence rule, ADR-0207 §6 for `mode`).
                    predecessor: None,
                    user: None,
                    initial_mode: DEFAULT_MODE.to_string(),
                    spawn_model: None,
                    inherited: None,
                    initial_workdir: None,
                },
                seqs2,
                activity2,
                forks,
            )
            .await;
        });
        self.sessions.insert(target.clone(), stx);
        Some(children)
    }
}
