use crate::session_store::{append, LogPayload, LogRecord};
use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::broadcast::error::RecvError;

/// Spawns a persistence subscriber that logs both inbound `InMsg` and outbound
/// `OutEvent` to disk, making each root session a self-contained resumable file.
///
/// It subscribes to the outbound event stream *and* the inbound message fan-out
/// (`holly.subscribe_inbound()`), so replay reconstructs user turns from the
/// logged `InMsg::Prompt` records — without them `Session::replay` folds zero
/// user messages and a resumed session appears to forget the conversation.
///
/// `InMsg::Resume` is skipped (it carries the whole prior log → recursion/bloat)
/// and `InMsg::Spawn` is skipped (it would create a stray single-line child file
/// that lists as a bogus root; a child's turns are already captured in the root
/// file via out events).
///
/// A `roots` map, folded from `SessionStarted { root, parent }`, routes every
/// record (root + spawned children) into the **root's** `{root_id}.jsonl`. Before
/// `SessionStarted` is seen the message's own session id is used as a fallback —
/// the first prompt arrives before its `SessionStarted`, but for a root session
/// the id is identical so the record lands in the same file either way.
pub fn spawn_persistence_subscriber(holly: &Holly, cwd: PathBuf) -> tokio::task::JoinHandle<()> {
    let mut events = holly.subscribe();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        // session id → its root session id.
        let mut roots: HashMap<SessionId, SessionId> = HashMap::new();
        // Each side closes independently on shutdown (the supervisor drops the
        // inbound sender when it exits, then sessions drop the outbound senders).
        // Keep draining the still-open side so buffered events aren't lost — a
        // one-shot `run` shuts the engine down the instant the turn ends.
        let mut in_open = true;
        let mut out_open = true;

        while in_open || out_open {
            // Bias toward inbound: a `Prompt` is causally before the outbound
            // events it produces, so draining inbound first keeps `In` records
            // ahead of their paired `Out` records on disk (record pairing on
            // replay assumes that order).
            tokio::select! {
                biased;

                in_msg = inbound.recv(), if in_open => match in_msg {
                    Ok(msg) => {
                        if matches!(msg, InMsg::Resume { .. } | InMsg::Spawn { .. }) {
                            continue;
                        }
                        let session_id = msg.session().clone();
                        let root = roots
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_else(|| session_id.clone());
                        let record = LogRecord::new(session_id, LogPayload::In(msg));
                        if let Err(e) = append(&cwd, &root, &record) {
                            tracing::error!("Failed to persist inbound message: {}", e);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} inbound messages");
                    }
                    Err(RecvError::Closed) => in_open = false,
                },

                out = events.recv(), if out_open => match out {
                    Ok(ev) => {
                        let session_id = ev.session().clone();
                        if let OutEvent::SessionStarted { session, parent, root, .. } = &ev {
                            let root_id = if *root {
                                session.clone()
                            } else {
                                parent
                                    .as_ref()
                                    .and_then(|p| roots.get(p).cloned())
                                    .unwrap_or_else(|| session.clone())
                            };
                            roots.insert(session.clone(), root_id);
                        }
                        let root = roots
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_else(|| session_id.clone());
                        let record = LogRecord::new(session_id, LogPayload::Out(ev));
                        if let Err(e) = append(&cwd, &root, &record) {
                            tracing::error!("Failed to persist outbound event: {}", e);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Persistence lagged, skipped {n} outbound events");
                    }
                    Err(RecvError::Closed) => out_open = false,
                },
            }
        }
    })
}
