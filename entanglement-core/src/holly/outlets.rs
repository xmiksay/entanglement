//! The two broadcast fan-outs as a [`Holly`][super::Holly] handle holds them,
//! plus the explicit shutdown switch (#700).
//!
//! A handle must not own a raw `broadcast::Sender`: the outbox would then stay
//! open for as long as *any* clone lives anywhere in the process (a detached
//! task, a `serve` connection, #699), and shutdown would only be as clean as
//! the least-disciplined holder. Handles share one `Outlets` instead;
//! [`Outlets::close`] takes the senders out so the channels close as soon as
//! the supervisor and its session tasks (which own their own sender clones)
//! have exited, regardless of how many handles are still alive.

use std::sync::{Mutex, PoisonError, RwLock};

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::protocol::{InMsg, OutEvent};

pub(super) struct Outlets {
    events: RwLock<Option<broadcast::Sender<OutEvent>>>,
    inbound: RwLock<Option<broadcast::Sender<InMsg>>>,
    shutdown: watch::Sender<bool>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

impl Outlets {
    pub(super) fn new(
        events: broadcast::Sender<OutEvent>,
        inbound: broadcast::Sender<InMsg>,
    ) -> (Self, watch::Receiver<bool>) {
        let (shutdown, rx) = watch::channel(false);
        let outlets = Self {
            events: RwLock::new(Some(events)),
            inbound: RwLock::new(Some(inbound)),
            shutdown,
            supervisor: Mutex::new(None),
        };
        (outlets, rx)
    }

    pub(super) fn set_supervisor(&self, handle: JoinHandle<()>) {
        *self
            .supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    /// Broadcast `ev`; a no-op once the engine has shut down.
    pub(super) fn emit(&self, ev: OutEvent) {
        if let Some(tx) = &*self.events.read().unwrap_or_else(PoisonError::into_inner) {
            let _ = tx.send(ev);
        }
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<OutEvent> {
        subscribe_or_closed(&self.events)
    }

    pub(super) fn subscribe_inbound(&self) -> broadcast::Receiver<InMsg> {
        subscribe_or_closed(&self.inbound)
    }

    /// Drop the handles' senders, tell the supervisor to stop, and hand back
    /// its join handle (`None` if another handle already shut down).
    pub(super) fn close(&self) -> Option<JoinHandle<()>> {
        self.events
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.inbound
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.shutdown.send_replace(true);
        self.supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// A subscriber after shutdown gets an already-closed receiver rather than a
/// panic or an `Option`, so every reader's existing `RecvError::Closed` arm is
/// the one shutdown path.
fn subscribe_or_closed<T: Clone>(
    slot: &RwLock<Option<broadcast::Sender<T>>>,
) -> broadcast::Receiver<T> {
    match &*slot.read().unwrap_or_else(PoisonError::into_inner) {
        Some(tx) => tx.subscribe(),
        None => broadcast::channel(1).1,
    }
}

/// Resolve once shutdown is requested. Every handle dropping without a
/// shutdown drops the watch sender too — that case stays pending forever so
/// the supervisor keeps its original exit path (drain the inbox, then stop on
/// its close).
pub(super) async fn requested(rx: &mut watch::Receiver<bool>) {
    if rx.wait_for(|&stop| stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}
