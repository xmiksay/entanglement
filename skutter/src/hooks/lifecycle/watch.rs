//! The watcher task: feeds the [`Tracker`] from the outbound and inbound
//! streams and runs its firings on two sequential workers — `post_tool_use`
//! on its own, so a slow one never delays an approval `notification`.

use std::sync::Arc;

use entanglement_core::{Holly, InMsg, OutEvent};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use super::{CallPaths, Firing, HookEvent, Hooks, Tracker};

/// Spawn the watcher when any observer point has a command (`None`
/// otherwise — the norm). Both receivers are subscribed here, before this
/// returns, so a first `Prompt` or `SessionStarted` is never missed. Abort the
/// handle to stop it (its two tasks live in a `JoinSet` dropped with it).
/// `paths` rewrites a `post_tool_use` payload's paths as the executor did.
pub fn spawn(holly: &Holly, hooks: Arc<Hooks>, paths: CallPaths) -> Option<JoinHandle<()>> {
    if !hooks.has_observers() {
        return None;
    }
    let events = holly.subscribe();
    let inbound = holly.subscribe_inbound();
    Some(tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        watch(&mut tasks, hooks, paths, events, inbound);
        while tasks.join_next().await.is_some() {}
    }))
}

/// The tracker feeding two sequential workers, so hooks run in event order
/// within each while a slow hook never lags the subscription. A busy batch's
/// `post_tool_use` firings (each up to `timeout_secs`) queue on their own
/// worker; every other point keeps its latency.
fn watch(
    tasks: &mut JoinSet<()>,
    hooks: Arc<Hooks>,
    paths: CallPaths,
    mut events: broadcast::Receiver<OutEvent>,
    mut inbound: broadcast::Receiver<InMsg>,
) {
    let tools = worker(tasks, hooks.clone());
    let lifecycle = worker(tasks, hooks.clone());
    tasks.spawn(async move {
        let mut tracker = Tracker {
            skip_execs: hooks.specs(HookEvent::PostToolUse).is_empty(),
            paths: Some(paths),
            ..Tracker::default()
        };
        let send = |f: Firing| {
            let tx = match f.event {
                HookEvent::PostToolUse => &tools,
                _ => &lifecycle,
            };
            hooks.specs(f.event).is_empty() || tx.send(f).is_ok()
        };
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                // Firings are best-effort under lag: dropped events fire
                // nothing. A pending turn end is forgotten rather than
                // classified off a `Status` whose `Done` may have been lost.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "hook watcher lagged");
                    tracker.forget_after_lag();
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            // The supervisor fans a frame out before handling it, so every
            // `Prompt`/`Resume` that caused this event is already buffered
            // here (a prompt's hook fires on the first event it causes).
            loop {
                match inbound.try_recv() {
                    Ok(msg) => {
                        if let Some(f) = tracker.observe_in(&msg) {
                            if !send(f) {
                                return;
                            }
                        }
                    }
                    Err(TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            if let Some(f) = tracker.observe(&ev) {
                if !send(f) {
                    break;
                }
            }
        }
    });
}

/// One sequential worker running the firings sent to it, in order.
fn worker(tasks: &mut JoinSet<()>, hooks: Arc<Hooks>) -> mpsc::UnboundedSender<Firing> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Firing>();
    tasks.spawn(async move {
        while let Some(f) = rx.recv().await {
            let tool = f.tool.as_deref();
            hooks
                .run_observer(f.event, &f.session, tool, &f.payload)
                .await;
        }
    });
    tx
}
