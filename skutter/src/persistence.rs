//! The file-backed persistence sink (#932): the library's tap
//! ([`spawn_persistence_subscriber_with_sink`]) writing into a
//! [`SessionStore`].

use std::sync::Arc;

use entanglement_core::{Holly, SessionId};
use entanglement_runtime::persistence::{spawn_persistence_subscriber_with_sink, RecordSink};

use crate::session_store::SessionStore;
use entanglement_runtime::session_store::LogRecord;

/// Appends each record to `{root_id}.jsonl` in a [`SessionStore`].
pub struct FileSink {
    store: SessionStore,
}

impl FileSink {
    pub fn new(store: SessionStore) -> Self {
        Self { store }
    }
}

impl RecordSink for FileSink {
    fn append(&self, root: &SessionId, record: &LogRecord) -> anyhow::Result<()> {
        self.store.append(root, record)
    }

    fn touch(&self, root: &SessionId) {
        if let Err(e) = self.store.touch(root) {
            tracing::warn!(%root, "could not refresh the session log's retention clock: {e:#}");
        }
    }
}

/// The library's persistence tap over a [`FileSink`] on `store`. A moved
/// sub-agent's history stays in its own root's file; the adopting root's
/// `Adopted` pointer is what [`SessionStore::read_tree`] follows on resume.
pub fn spawn_persistence_subscriber(
    holly: &Holly,
    store: SessionStore,
) -> tokio::task::JoinHandle<()> {
    spawn_persistence_subscriber_with_sink(holly, Arc::new(FileSink::new(store)))
}
