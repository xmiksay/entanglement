//! [`AgentRegistry`] — the shared table of launched sub-agents, keyed by their
//! handle (a child `SessionId`, minted by `agent_spawn`/`agent`). Records each
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

/// One tracked sub-agent: when it launched, which session spawned it, and a
/// watch handle to observe its completion. The launch watcher owns the
/// [`watch::Sender`]; every entry keeps a receiver so the last value survives
/// the sender being dropped, letting a late poll still read a completed answer.
#[derive(Clone)]
struct Entry {
    started: Instant,
    parent: SessionId,
    status: watch::Receiver<AgentStatus>,
}

/// Shared table of launched sub-agents keyed by child `SessionId` (the handle
/// `agent_spawn` returns). Cloned into every launch/poll task — the `Arc<Mutex>`
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
    /// Register a freshly-launched child as `Running`, owned by `parent`.
    /// Returns the sender the launch watcher flips to `Complete`, plus the
    /// launch instant so it can report the same elapsed a poller would compute.
    pub fn register(
        &self,
        child: SessionId,
        parent: SessionId,
    ) -> (watch::Sender<AgentStatus>, Instant) {
        let (tx, rx) = watch::channel(AgentStatus::Running);
        let started = Instant::now();
        self.lock().insert(
            child,
            Entry {
                started,
                parent,
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
        let (tx, _started) = reg.register(child.clone(), parent.clone());
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
        reg.register(child.clone(), parent.clone());
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
        reg.register(child.clone(), parent.clone());

        assert!(reg.view(&stranger, &child).is_none());
        assert!(reg.view(&parent, &child).is_some());
    }
}
