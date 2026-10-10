//! Engine-state folds a revert depends on: which root a session belongs to,
//! and whether a session is still doing work that could write files.

use std::sync::Arc;

use entanglement_core::{AgentState, Holly, OutEvent, SessionId};
use tokio::sync::broadcast::error::RecvError;

use super::{lock, FileBackups};
use crate::host::jobs::{JobRegistry, JobStatus};
use crate::script_ops::ScriptRegistry;

/// "Is this session still running detached work?" — a background job or a
/// detached `rhai` script, which no `Status` reflects. Consulted for every
/// session of a revert's sub-tree.
pub type BusyProbe = Arc<dyn Fn(&SessionId) -> bool + Send + Sync>;

/// The [`BusyProbe`] over the process's background-job and detached-script
/// registries: a session with a running job or script may still write.
pub fn detached_work_probe(jobs: JobRegistry, scripts: ScriptRegistry) -> BusyProbe {
    Arc::new(move |session| {
        jobs.snapshot(Some(session))
            .iter()
            .any(|j| matches!(j.status, JobStatus::Running))
            || scripts
                .snapshot_ops(Some(session))
                .iter()
                .any(|s| s.running)
    })
}

impl FileBackups {
    /// Fold `holly`'s lifecycle into this handle: `SessionStarted` lineage
    /// (the persistence tap's root rule) so a child's backups and reverts
    /// resolve to its root, and `Status` so a revert can refuse while any
    /// session of its sub-tree is mid-turn.
    pub fn track_roots(&self, holly: &Holly) -> tokio::task::JoinHandle<()> {
        let mut events = holly.subscribe();
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => this.fold(&ev),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
        })
    }

    fn fold(&self, ev: &OutEvent) {
        match ev {
            OutEvent::SessionStarted {
                session,
                parent,
                root,
                ..
            } => {
                let root_id = match (root, parent) {
                    (false, Some(p)) => self.root_of(p),
                    _ => session.clone(),
                };
                self.note_root(session.clone(), root_id);
            }
            OutEvent::Status { session, state } => {
                let mut busy = lock(&self.inner.busy);
                if matches!(
                    state,
                    AgentState::Idle | AgentState::Done | AgentState::Error
                ) {
                    busy.remove(session);
                } else {
                    busy.insert(session.clone());
                }
            }
            OutEvent::SessionEnded { session, .. }
            | OutEvent::SessionHibernated { session, .. } => {
                lock(&self.inner.busy).remove(session);
            }
            _ => {}
        }
    }

    /// Record that `session` belongs to root `root`.
    pub fn note_root(&self, session: SessionId, root: SessionId) {
        lock(&self.inner.roots).insert(session, root);
    }

    /// Install the detached-work check (once; later calls are ignored).
    pub fn set_busy_probe(&self, probe: BusyProbe) {
        let _ = self.inner.probe.set(probe);
    }

    pub(super) fn root_of(&self, session: &SessionId) -> SessionId {
        lock(&self.inner.roots)
            .get(session)
            .cloned()
            .unwrap_or_else(|| session.clone())
    }

    /// Whether `session` may still write: mid-turn per its last `Status`, or
    /// running detached work per the probe.
    pub(super) fn is_busy(&self, session: &SessionId) -> bool {
        lock(&self.inner.busy).contains(session)
            || self.inner.probe.get().is_some_and(|probe| probe(session))
    }

    #[cfg(test)]
    pub(super) fn fold_for_test(&self, ev: &OutEvent) {
        self.fold(ev);
    }
}
