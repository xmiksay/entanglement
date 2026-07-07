use crate::session_store::{append, LogPayload, LogRecord};
use entanglement_core::{Holly, OutEvent, SessionId};
use std::path::PathBuf;

/// Spawns a persistence subscriber that logs all OutEvent to disk.
///
/// This subscriber runs in the background and persists events to the session store.
/// It tracks the current root session ID from SessionStarted events and uses it
/// to determine which file to write to.
///
/// Note: InMsg persistence is not yet implemented. A proper implementation would
/// require intercepting Holly::send calls or restructuring the supervisor to
/// broadcast inbound messages.
pub fn spawn_persistence_subscriber(holly: &Holly, cwd: PathBuf) -> tokio::task::JoinHandle<()> {
    let mut events = holly.subscribe();

    tokio::spawn(async move {
        let mut root_session_id: Option<SessionId> = None;

        while let Ok(ev) = events.recv().await {
            let session_id = ev.session().clone();

            // Track root session ID from SessionStarted events
            if let OutEvent::SessionStarted { root: true, .. } = &ev {
                root_session_id = Some(session_id.clone());
            }

            // Write outbound events to the log
            if let Some(root) = &root_session_id {
                let record = LogRecord::new(session_id, LogPayload::Out(ev));
                if let Err(e) = append(&cwd, root, &record) {
                    tracing::error!("Failed to persist outbound event: {}", e);
                }
            }
        }
    })
}
