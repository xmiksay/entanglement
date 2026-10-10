//! The unknown-session policy ([ADR-0210](../../../docs/adr/0210-unknown-session-policy.md)):
//! what the supervisor does with a routable message addressed to a session id
//! that has no live task. Public seam types only; the behavior lives in
//! `supervisor/unknown.rs`.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::protocol::{InMsg, OutEvent, SessionId};

/// Fetches a session's persisted log so the engine can rebuild it on demand
/// ([`UnknownSession::Resume`]). Implemented by the embedder over its own
/// store — core owns no persistence.
#[async_trait]
pub trait SessionLoader: Send + Sync {
    /// Load the replay records for `session`, in exactly the shape
    /// [`Holly::resume`][crate::Holly::resume] consumes — the root's whole log
    /// (a spawned child's records fold into its root's, ADR-0020), paired
    /// `(inbound, outbound)` the way the runtime's `pair_records` does.
    ///
    /// `Ok(None)` means "no such session": the message is refused exactly as
    /// under [`UnknownSession::Refuse`]. `Err` surfaces as an
    /// [`OutEvent::Error`] and spawns nothing.
    ///
    /// WHY bound your own I/O: every message for `session` queues behind this
    /// call until it returns, so a load that never completes strands them.
    async fn load(
        &self,
        session: &SessionId,
    ) -> anyhow::Result<Option<Vec<(Option<InMsg>, OutEvent)>>>;
}

/// What a routable message (`Prompt`, `ToolResult`, `Stop`, `SetMode`, …)
/// addressed to a session id with no live task does. `Spawn` and `Resume`
/// always create explicitly and are unaffected; see ADR-0210 for the full
/// coverage table.
#[derive(Clone, Default)]
pub enum UnknownSession {
    /// Lazily create a blank root session under the default agent — the
    /// single-user convenience every in-tree head relies on.
    #[default]
    SpawnBlank,
    /// Spawn nothing; answer with a seq-`0` [`OutEvent::Error`] naming the
    /// session (ADR-0072's lifecycle bypass) and drop the message.
    Refuse,
    /// Rebuild the session (and its still-live spawn sub-tree, ADR-0112) from
    /// the embedder's log, then deliver the message. Concurrent messages for
    /// the same id queue behind one in-flight load and deliver in arrival
    /// order.
    Resume(Arc<dyn SessionLoader>),
}

impl fmt::Debug for UnknownSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SpawnBlank => "SpawnBlank",
            Self::Refuse => "Refuse",
            Self::Resume(_) => "Resume(..)",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    #[async_trait]
    impl SessionLoader for Nothing {
        async fn load(
            &self,
            _: &SessionId,
        ) -> anyhow::Result<Option<Vec<(Option<InMsg>, OutEvent)>>> {
            Ok(None)
        }
    }

    #[test]
    fn default_keeps_the_single_user_blank_spawn() {
        assert!(matches!(
            UnknownSession::default(),
            UnknownSession::SpawnBlank
        ));
    }

    #[test]
    fn debug_never_needs_the_loader_to_be_debug() {
        let policy = UnknownSession::Resume(Arc::new(Nothing));
        assert_eq!(format!("{policy:?}"), "Resume(..)");
        assert_eq!(format!("{:?}", UnknownSession::Refuse), "Refuse");
    }
}
