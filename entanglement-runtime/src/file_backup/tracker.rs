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

/// "Which sessions is this one under, right now?" — `session` and its live
/// ancestors, nearest first, or empty when the session is unknown (a session
/// already closed or hibernated, whose change then goes by the logs, ADR-0236
/// §8). Asked before every recorded write, so the change carries the tree it
/// was made in. A chain cut short by an ancestor that is not (or no longer)
/// in the directory ends at that ancestor: still a true prefix of the path,
/// so every session it names did hold the change.
pub type AncestryProbe = Arc<dyn Fn(&SessionId) -> Vec<SessionId> + Send + Sync>;

/// The [`AncestryProbe`] over `holly`'s session directory. The supervisor
/// re-parents a moved sub-agent there (ADR-0234) *before* it broadcasts
/// `ParentChanged`, under the directory's lock, so a write stamped after the
/// move reads the new parent with no broadcast consumer in between.
pub fn holly_ancestry(holly: Holly) -> AncestryProbe {
    Arc::new(move |session| {
        ancestry_walk(session, |s| holly.session_info(s).map(|info| info.parent))
    })
}

/// Walk `session`'s parents through `parent_of` (`None` = unknown session,
/// `Some(None)` = a root): see [`AncestryProbe`].
pub(super) fn ancestry_walk(
    session: &SessionId,
    parent_of: impl Fn(&SessionId) -> Option<Option<SessionId>>,
) -> Vec<SessionId> {
    let Some(mut parent) = parent_of(session) else {
        return Vec::new();
    };
    let mut out = vec![session.clone()];
    // Bounded, so a corrupted (cyclic) directory can't spin forever.
    for _ in 0..64 {
        let Some(p) = parent else {
            break;
        };
        out.push(p.clone());
        match parent_of(&p) {
            Some(next) => parent = next,
            None => break,
        }
    }
    out
}

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
        self.set_ancestry_probe(holly_ancestry(holly.clone()));
        let mut events = holly.subscribe();
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => this.fold(&ev),
                    Err(RecvError::Lagged(_)) => this.poison(),
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

    /// A lagged subscription may have dropped a `Status` that started a turn:
    /// treat every known session as busy until it reports a fresh `Status`
    /// (or ends) — refusing is safe, racing a write is not.
    fn poison(&self) {
        let known: Vec<SessionId> = lock(&self.inner.roots).keys().cloned().collect();
        lock(&self.inner.busy).extend(known);
    }

    /// Record that `session` belongs to root `root`.
    pub fn note_root(&self, session: SessionId, root: SessionId) {
        lock(&self.inner.roots).insert(session, root);
    }

    /// Install the ancestry lookup each recorded change carries (once; later
    /// calls are ignored). [`track_roots`](Self::track_roots) installs
    /// Holly's; without one, changes are recorded without it (legacy).
    pub fn set_ancestry_probe(&self, probe: AncestryProbe) {
        let _ = self.inner.ancestry.set(probe);
    }

    pub(super) fn ancestry_of(&self, session: &SessionId) -> Vec<SessionId> {
        self.inner
            .ancestry
            .get()
            .map(|probe| probe(session))
            .unwrap_or_default()
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

    /// Whether `session` may still write: mid-turn per its last `Status`
    /// (skipped for the revert's own `target`, which core only lets revert
    /// while idle — a stale mark must not lock it out), or running detached
    /// work per the probe.
    pub(super) fn is_busy(&self, session: &SessionId, target: &SessionId) -> bool {
        (session != target && lock(&self.inner.busy).contains(session))
            || self.inner.probe.get().is_some_and(|probe| probe(session))
    }

    #[cfg(test)]
    pub(super) fn fold_for_test(&self, ev: &OutEvent) {
        self.fold(ev);
    }

    #[cfg(test)]
    pub(super) fn poison_for_test(&self) {
        self.poison();
    }
}
