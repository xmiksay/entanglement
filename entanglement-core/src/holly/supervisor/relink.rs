//! Keeping the spawn tree whole across a compaction fork (ADR-0234): a
//! successor takes its predecessor's place, and the predecessor's live
//! sub-agents move under the successor instead of dying in its close cascade.

use super::Supervisor;
use crate::protocol::{OutEvent, SessionId};
use crate::session::SessionCmd;

impl Supervisor {
    /// The parent a spawning session is placed under. An explicit parent
    /// with a live successor resolves to it — even while the source is still
    /// registered, between the fork's `Spawn` and its `CloseSession`, where a
    /// child placed under the source would die in the cascade. A parentless
    /// compaction successor inherits its predecessor's parent, so a
    /// sub-agent that compacts stays in its parent's sub-tree.
    pub(super) fn placed_parent(
        &self,
        parent: Option<&SessionId>,
        predecessor: Option<&SessionId>,
    ) -> Option<SessionId> {
        match (parent, predecessor) {
            (Some(p), _) => Some(
                self.session_meta
                    .successor_of(p)
                    .filter(|s| self.sessions.contains_key(s))
                    .unwrap_or_else(|| p.clone()),
            ),
            (None, Some(source)) => self.parent_links.get(source).cloned().flatten(),
            (None, None) => None,
        }
    }

    /// Move `source`'s live direct children under `successor`. Runs while the
    /// successor's `Spawn` is handled, and the fork sends the source's
    /// `CloseSession` only after that `Spawn` on the same channel, so the
    /// supervisor's single-threaded loop never lets the close cascade see a
    /// child that is about to move. Only children with a running task move:
    /// a closed one already left `parent_links`, and a hibernated one has no
    /// task to keep, so the cascade still retires it as before.
    pub(super) async fn adopt_children(&mut self, source: &SessionId, successor: &SessionId) {
        let mut children: Vec<SessionId> = self
            .parent_links
            .iter()
            .filter(|(c, p)| p.as_ref() == Some(source) && self.sessions.contains_key(*c))
            .map(|(c, _)| c.clone())
            .collect();
        // Deterministic event order for heads and tests.
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for child in children {
            self.parent_links
                .insert(child.clone(), Some(successor.clone()));
            self.session_meta.set_parent(&child, successor);
            tracing::info!(%child, %source, %successor, "sub-agent re-parented to the compaction successor");
            let _ = self.events.send(OutEvent::ParentChanged {
                session: child.clone(),
                parent: successor.clone(),
            });
            if let Some(tx) = self.sessions.get(successor) {
                let _ = tx.send(SessionCmd::ChildSpawned(child)).await;
            }
        }
    }
}
