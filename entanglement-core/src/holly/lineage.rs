//! Compaction lineage reads on [`Holly`] (ADR-0205, ADR-0234), served from
//! the supervisor's directory rather than the lossy broadcast.

use super::Holly;
use crate::protocol::SessionId;

impl Holly {
    /// The newest session `session` compacted into (ADR-0205), `None` if it
    /// never did. Written before the successor's `SessionStarted`, so a
    /// consumer that lost lifecycle events still finds it (ADR-0234); check
    /// [`session_info`][Self::session_info] for whether it is still live.
    /// After a restart it covers every lineage a resume replayed.
    pub fn successor_of(&self, session: &SessionId) -> Option<SessionId> {
        self.directory.successor_of(session)
    }

    /// The oldest session in `session`'s compaction lineage — the id it had
    /// before any compaction, which is the handle a parent was given for a
    /// sub-agent (ADR-0234). `session` itself when it never succeeded one.
    pub fn lineage_head(&self, session: &SessionId) -> SessionId {
        self.directory.lineage_head(session)
    }
}
