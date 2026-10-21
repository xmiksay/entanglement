//! The unknown-session policy ([ADR-0210](../../../../docs/adr/0210-unknown-session-policy.md)):
//! a routable message for an id with no live task spawns a blank root, is
//! refused, or waits on the embedder's [`SessionLoader`] and is delivered once
//! the session is resumed.
//!
//! Concurrency: the supervisor is one loop, so the load runs on its own task
//! and reports back over `load_tx`. While it is in flight, `pending_loads`
//! holds the id's queue — every further routable message for that id is
//! appended there instead of starting a second load, and the completion
//! replays the queue through [`Supervisor::handle`] in arrival order.

use std::collections::HashSet;
use std::sync::Arc;

use super::ancestry::logged_parent;
use super::Supervisor;
use crate::holly::routing::emit_supervisor_error;
use crate::holly::{SessionLoader, UnknownSession};
use crate::protocol::{InMsg, OutEvent, SessionId};

/// A finished [`SessionLoader::load`] call, sent back to the supervisor loop.
pub(super) type LoadDone = (
    SessionId,
    anyhow::Result<Option<Vec<(Option<InMsg>, OutEvent)>>>,
);

pub(super) const CLOSED_ID: &str =
    "session id is closed (ids are single-use); mint a fresh session id";

/// Refusals for a load that finished under a down ancestor (ADR-0246).
pub(super) const PARENT_HIBERNATED: &str =
    "an ancestor of this session was hibernated while it loaded; resume the ancestor first";
pub(super) const HIBERNATED_WHILE_LOADING: &str =
    "this session was hibernated while it loaded; resume it to address it again";
pub(super) const PARENT_CLOSED: &str =
    "an ancestor of this session is closed; its sub-tree cannot be resumed";

/// One in-flight load: the messages queued behind it, and every id an
/// ancestor hibernate evicted meanwhile.
#[derive(Default)]
pub(super) struct PendingLoad {
    pub(super) queued: Vec<InMsg>,
    pub(super) hibernated: HashSet<SessionId>,
}

impl Supervisor {
    /// Admit a routable `msg` for `session_id`, which has no live task (or
    /// still has a load draining). Returns the message when the session is
    /// live and it should route now; `None` when it was refused or queued.
    pub(super) fn admit_unknown(&mut self, session_id: &SessionId, msg: InMsg) -> Option<InMsg> {
        if let Some(load) = self.pending_loads.get_mut(session_id) {
            load.queued.push(msg);
            return None;
        }
        // A closed id is spent under every policy (ADR-0028, issue #105): a
        // frame that raced behind its `CloseSession` must never resurrect it.
        if self.closed.contains(session_id) {
            self.refuse_queued(session_id, 1, CLOSED_ID);
            return None;
        }
        // Cloned (an `Arc` at most) so the arms below can borrow `self` mutably.
        match self.cfg.unknown_session.clone() {
            UnknownSession::SpawnBlank => self.spawn_lazy_root(session_id).then_some(msg),
            UnknownSession::Refuse => {
                self.refuse_unknown(session_id, 1);
                None
            }
            UnknownSession::Resume(loader) => {
                self.start_load(session_id, loader);
                let load = PendingLoad {
                    queued: vec![msg],
                    ..PendingLoad::default()
                };
                self.pending_loads.insert(session_id.clone(), load);
                None
            }
        }
    }

    /// Run `loader` for `session_id` off the supervisor loop. A panicking
    /// loader is reported as an `Err` so the id's queue is never stranded by
    /// it.
    fn start_load(&self, session_id: &SessionId, loader: Arc<dyn SessionLoader>) {
        let tx = self.load_tx.clone();
        let id = session_id.clone();
        tokio::spawn(async move {
            let target = id.clone();
            let result = match tokio::spawn(async move { loader.load(&target).await }).await {
                Ok(result) => result,
                Err(e) => Err(anyhow::anyhow!("session loader panicked: {e}")),
            };
            // A closed channel means the supervisor is gone; nothing to deliver.
            let _ = tx.send((id, result)).await;
        });
    }

    /// Apply a finished load: resume the session if it is still not live,
    /// then deliver its queued messages in order — or refuse them all.
    pub(super) async fn finish_load(&mut self, (session_id, result): LoadDone) {
        // No queue ⇒ a `CloseSession` abandoned this load; apply nothing.
        let Some(PendingLoad { queued, hibernated }) = self.pending_loads.remove(&session_id)
        else {
            return;
        };
        // An explicit `Resume`/`Spawn` may have made it live meanwhile; the
        // loaded records are then redundant and only the queue matters.
        if !self.sessions.contains_key(&session_id) {
            match result {
                Ok(Some(records)) => {
                    if let Some(reason) = self.down_ancestor(&session_id, &records, &hibernated) {
                        self.refuse_queued(&session_id, queued.len(), reason);
                        return;
                    }
                    self.resume(&session_id, &records).await
                }
                Ok(None) => {
                    self.refuse_unknown(&session_id, queued.len());
                    return;
                }
                Err(e) => {
                    tracing::warn!(%session_id, "session loader failed: {e:#}");
                    let reason = format!("failed to load session `{session_id}`: {e:#}");
                    self.refuse_queued(&session_id, queued.len(), &reason);
                    return;
                }
            }
            // A refused or failed replay already emitted its own `Error`.
            if !self.sessions.contains_key(&session_id) {
                return;
            }
        }
        for msg in queued {
            self.handle(msg).await;
        }
    }

    /// Why a loaded session must not start: it was itself hibernated while
    /// it loaded, an ancestor is closed, or one was hibernated while it
    /// loaded and is still down (ADR-0246). A load
    /// that merely finds its ancestors not live (a fresh process, a child
    /// whose tree was hibernated before) starts as an explicit `Resume`
    /// would.
    fn down_ancestor(
        &self,
        session_id: &SessionId,
        records: &[(Option<InMsg>, OutEvent)],
        hibernated: &HashSet<SessionId>,
    ) -> Option<&'static str> {
        if hibernated.contains(session_id) {
            return Some(HIBERNATED_WHILE_LOADING);
        }
        let parent = logged_parent(records, session_id);
        let chain = self.ancestors(parent, records);
        if chain.iter().any(|a| self.closed.contains(a)) {
            return Some(PARENT_CLOSED);
        }
        chain
            .iter()
            .any(|a| hibernated.contains(a) && !self.sessions.contains_key(a))
            .then_some(PARENT_HIBERNATED)
    }

    fn refuse_unknown(&self, session_id: &SessionId, count: usize) {
        let reason =
            format!("unknown session `{session_id}`: not live and not spawned (message dropped)");
        self.refuse_queued(session_id, count, &reason);
    }

    /// One `Error` per dropped message (seq `0` unless the registry still keeps
    /// the id's retired seq cell, ADR-0239), so a head counting its sends sees every one
    /// accounted for.
    pub(super) fn refuse_queued(&self, session_id: &SessionId, count: usize, reason: &str) {
        for _ in 0..count {
            emit_supervisor_error(&self.events, &self.seqs, session_id, reason);
        }
    }
}
