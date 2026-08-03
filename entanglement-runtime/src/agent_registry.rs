//! [`AgentRegistry`] — the shared table of launched sub-agents, keyed by their
//! handle (a child `SessionId`, minted by `agent`). Records each
//! child's completion so a later `poll` (#605, formerly `agent_poll`, ADR-0026/
//! ADR-0161) can collect the answer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use entanglement_core::SessionId;
use tokio::sync::watch;

/// Live status of a launched sub-agent, surfaced through [`AgentRegistry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// The child is still running; no answer yet.
    Running,
    /// The child finished — carries its final answer and how long it ran
    /// (from the `Spawn` send to the child's `Done`).
    Complete { answer: String, elapsed: Duration },
}

/// One tracked sub-agent: when it launched, which session spawned it, under
/// which profile, and a watch handle to observe its completion. The launch
/// watcher owns the [`watch::Sender`]; every entry keeps a receiver so the
/// last value survives the sender being dropped, letting a late poll still
/// read a completed answer.
#[derive(Clone)]
struct Entry {
    started: Instant,
    parent: SessionId,
    /// The profile the child was launched under — "what launched it" in a
    /// #607 pending-operations listing.
    agent: String,
    status: watch::Receiver<AgentStatus>,
}

/// One outstanding child launched by a session, as reported to a #607
/// pending-operations listing.
pub struct AgentOpInfo {
    pub session: SessionId,
    pub handle: String,
    pub agent: String,
    pub elapsed: Duration,
    pub status: AgentStatus,
}

/// Shared table of launched sub-agents keyed by child `SessionId` (the handle
/// a `background: true` `agent` call returns). Cloned into every launch/poll task — the `Arc<Mutex>`
/// is only ever held briefly to insert or clone a receiver, never across an
/// `.await`, so pollers block on the watch channel, not the lock.
///
/// Scoped by spawning parent (#618): a handle is only ever handed back to the
/// session that launched it, so [`Self::view`] only resolves a lookup made by
/// that same parent — any other session's poll (even one that guesses or
/// otherwise learns the id) is treated as unknown, exactly like a poll for an
/// id that was never launched.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    inner: Arc<Mutex<HashMap<SessionId, Entry>>>,
}

impl AgentRegistry {
    /// Register a freshly-launched child as `Running`, owned by `parent` and
    /// launched under the `agent` profile. Returns the sender the launch
    /// watcher flips to `Complete`, plus the launch instant so it can report
    /// the same elapsed a poller would compute.
    pub fn register(
        &self,
        child: SessionId,
        parent: SessionId,
        agent: String,
    ) -> (watch::Sender<AgentStatus>, Instant) {
        let (tx, rx) = watch::channel(AgentStatus::Running);
        let started = Instant::now();
        self.lock().insert(
            child,
            Entry {
                started,
                parent,
                agent,
                status: rx,
            },
        );
        (tx, started)
    }

    /// Drop a child that never actually launched (the `Spawn` send failed), so a
    /// stray handle can't linger as perpetually `Running`.
    pub fn forget(&self, child: &SessionId) {
        self.lock().remove(child);
    }

    /// A poller's view of `child`, as seen by `poller`: its launch instant and
    /// a fresh receiver. `None` when no such handle was ever launched *by
    /// `poller`* — either it doesn't exist at all, or it belongs to a
    /// different session, both of which the caller must treat identically to
    /// avoid confirming another session's handle exists.
    pub fn view(
        &self,
        poller: &SessionId,
        child: &SessionId,
    ) -> Option<(Instant, watch::Receiver<AgentStatus>)> {
        self.lock()
            .get(child)
            .and_then(|e| (&e.parent == poller).then(|| (e.started, e.status.clone())))
    }

    /// Snapshot launched children (#607, ADR-0161 §6) — the no-handle listing
    /// `poll`/`InMsg::ListOperations` answer with. `parent`, when set, scopes
    /// to that launcher only (the `poll` tool's own use, always scoped to the
    /// calling session); `None` spans every parent, mirroring
    /// [`crate::questions::OpenQuestions::snapshot`]'s filter convention.
    /// Unlike [`JobRegistry`][crate::host::jobs::JobRegistry], a completed
    /// entry is never evicted here, so it stays listed until the model polls
    /// it by handle — the model sees an unclaimed answer sitting there rather
    /// than losing track of it. Sorted by handle for a deterministic reply.
    pub fn snapshot(&self, parent: Option<&SessionId>) -> Vec<AgentOpInfo> {
        let mut list: Vec<AgentOpInfo> = self
            .lock()
            .iter()
            .filter(|(_, e)| parent.is_none_or(|p| p == &e.parent))
            .map(|(child, e)| AgentOpInfo {
                session: e.parent.clone(),
                handle: child.to_string(),
                agent: e.agent.clone(),
                elapsed: e.started.elapsed(),
                status: e.status.borrow().clone(),
            })
            .collect();
        list.sort_by(|a, b| a.handle.cmp(&b.handle));
        list
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Entry>> {
        // Poisoning only happens if a holder panicked while mutating the map;
        // we never panic under the lock, so this is provably unreachable.
        self.inner.lock().expect("agent registry mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn view_none_for_unknown_handle() {
        let reg = AgentRegistry::default();
        assert!(reg
            .view(&SessionId::new("parent"), &SessionId::new("nope"))
            .is_none());
    }

    #[tokio::test]
    async fn complete_is_readable_after_sender_dropped() {
        // A poll that arrives *after* the child finished (and the launch task
        // dropped its sender) must still read the completed answer.
        let reg = AgentRegistry::default();
        let parent = SessionId::new("p1");
        let child = SessionId::new("c1");
        let (tx, _started) = reg.register(child.clone(), parent.clone(), "build".to_string());
        tx.send(AgentStatus::Complete {
            answer: "done".to_string(),
            elapsed: Duration::from_millis(3),
        })
        .unwrap();
        drop(tx);

        let (_started, mut rx) = reg.view(&parent, &child).expect("entry present");
        rx.changed().await.ok();
        let status = rx.borrow().clone();
        assert_eq!(
            status,
            AgentStatus::Complete {
                answer: "done".to_string(),
                elapsed: Duration::from_millis(3),
            }
        );
    }

    #[tokio::test]
    async fn forget_removes_a_failed_launch() {
        let reg = AgentRegistry::default();
        let parent = SessionId::new("p3");
        let child = SessionId::new("c3");
        reg.register(child.clone(), parent.clone(), "build".to_string());
        reg.forget(&child);
        assert!(reg.view(&parent, &child).is_none());
    }

    #[tokio::test]
    async fn view_refuses_a_poll_from_a_non_owning_session() {
        // #618: the handle is only ever handed back to the session that
        // launched it — a different session that happens to know (or guess)
        // the agent_id must be treated exactly like an unknown handle.
        let reg = AgentRegistry::default();
        let parent = SessionId::new("owner");
        let stranger = SessionId::new("stranger");
        let child = SessionId::new("c4");
        reg.register(child.clone(), parent.clone(), "build".to_string());

        assert!(reg.view(&stranger, &child).is_none());
        assert!(reg.view(&parent, &child).is_some());
    }

    #[tokio::test]
    async fn snapshot_lists_only_the_parent_s_own_children() {
        let reg = AgentRegistry::default();
        let parent = SessionId::new("p5");
        let stranger = SessionId::new("stranger5");
        let child = SessionId::new("c5");
        reg.register(child.clone(), parent.clone(), "reviewer".to_string());

        let mine = reg.snapshot(Some(&parent));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].session, parent);
        assert_eq!(mine[0].handle, child.to_string());
        assert_eq!(mine[0].agent, "reviewer");
        assert_eq!(mine[0].status, AgentStatus::Running);

        assert!(reg.snapshot(Some(&stranger)).is_empty());
        assert_eq!(reg.snapshot(None).len(), 1);
    }

    #[tokio::test]
    async fn snapshot_still_lists_a_completed_child() {
        // Unlike a job, a completed agent entry isn't evicted — it stays
        // listed until the model polls it by handle (#607).
        let reg = AgentRegistry::default();
        let parent = SessionId::new("p6");
        let child = SessionId::new("c6");
        let (tx, _started) = reg.register(child.clone(), parent.clone(), "build".to_string());
        tx.send(AgentStatus::Complete {
            answer: "done".to_string(),
            elapsed: Duration::from_millis(3),
        })
        .unwrap();

        let mine = reg.snapshot(Some(&parent));
        assert_eq!(mine.len(), 1);
        assert_eq!(
            mine[0].status,
            AgentStatus::Complete {
                answer: "done".to_string(),
                elapsed: Duration::from_millis(3),
            }
        );
    }
}
