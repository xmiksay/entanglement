//! Per-session cancellation of in-flight tool tasks (#167).
//!
//! Core parks a turn as data and, on [`InMsg::Stop`], simply clears that state —
//! it never owns the executing tool. So a `Stop` that lands while a `bash`/`call`
//! command or a `rhai` script is running left the work going: the detached
//! executor task kept draining the child, and a CPU-bound Rhai engine (running
//! under an un-abortable `spawn_blocking`) ran to completion.
//!
//! This registry closes that gap. Each in-flight tool task registers a
//! [`TaskCanceller`] under its session before it starts and the executor's
//! inbound watcher aborts every one of them when a `Stop` for that session
//! arrives on the fan-out. Aborting the async task drops its future, which for
//! `bash`/`call` fires the exec tool's process-group SIGKILL guard (so
//! grandchildren don't orphan — #168); for `rhai` the abort is paired with a
//! cooperative stop flag the blocking engine's progress callback polls, because
//! an aborted `spawn_blocking` keeps running otherwise.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use entanglement_core::SessionId;
use tokio::task::AbortHandle;

/// Cancellation handle for one in-flight tool task.
pub struct TaskCanceller {
    abort: AbortHandle,
    /// A blocking script engine's cooperative stop flag ([`crate::script`]). An
    /// aborted `spawn_blocking` keeps running, so the engine only unwinds once
    /// its progress callback observes this — and, unlike a thrown binding error,
    /// that termination cannot be `try`/`catch`-swallowed by the script (#167).
    stop_flag: Option<Arc<AtomicBool>>,
}

impl TaskCanceller {
    /// A regular async tool task: aborting its future is enough (the exec tools'
    /// own drop guard turns that into a process-group kill — #168).
    pub fn task(abort: AbortHandle) -> Self {
        Self {
            abort,
            stop_flag: None,
        }
    }

    /// A `rhai` script task: pair the abort with the engine's stop flag so the
    /// un-abortable blocking thread also unwinds.
    pub fn script(abort: AbortHandle, stop_flag: Arc<AtomicBool>) -> Self {
        Self {
            abort,
            stop_flag: Some(stop_flag),
        }
    }

    fn cancel(&self) {
        // Trip the cooperative flag first so the blocking engine is already
        // unwinding by the time the async task's abort takes effect.
        if let Some(flag) = &self.stop_flag {
            flag.store(true, Ordering::SeqCst);
        }
        self.abort.abort();
    }
}

/// Tracks the in-flight tool tasks per session so an `InMsg::Stop` for that
/// session can abort them. Cheap to clone (shared `Arc<Mutex<_>>`): the executor
/// loop registers new tasks, the inbound watcher cancels them.
#[derive(Clone, Default)]
pub struct CancelRegistry {
    inner: Arc<Mutex<HashMap<SessionId, Vec<TaskCanceller>>>>,
}

impl CancelRegistry {
    /// Record an in-flight task under `session`. Finished tasks accumulated for
    /// the session are pruned here — an `AbortHandle` is tiny and abort on a
    /// finished task is a no-op, so lazy pruning keeps the map bounded without a
    /// per-task self-deregistration race.
    pub fn register(&self, session: &SessionId, canceller: TaskCanceller) {
        let mut map = self.inner.lock().unwrap();
        let tasks = map.entry(session.clone()).or_default();
        tasks.retain(|c| !c.abort.is_finished());
        tasks.push(canceller);
    }

    /// Abort every in-flight tool task for `session` (its `Stop` arrived).
    pub fn cancel_session(&self, session: &SessionId) {
        // Take the vec out under the lock, then cancel outside it: `abort()` only
        // signals (the runtime drops the future), so there is no re-entry, but
        // keeping the lock scope tight avoids holding it across the loop.
        let tasks = self.inner.lock().unwrap().remove(session);
        if let Some(tasks) = tasks {
            for c in &tasks {
                c.cancel();
            }
        }
    }

    /// Drop a session's bookkeeping when it ends (no cancellation).
    pub fn forget_session(&self, session: &SessionId) {
        self.inner.lock().unwrap().remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_session_aborts_registered_task() {
        let reg = CancelRegistry::default();
        let session = SessionId::new("s1");
        let started = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));

        let s = started.clone();
        let c = completed.clone();
        let handle = tokio::spawn(async move {
            s.store(true, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            c.store(true, Ordering::SeqCst);
        });
        reg.register(&session, TaskCanceller::task(handle.abort_handle()));

        // Let the task start, then cancel before its sleep elapses.
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        reg.cancel_session(&session);

        let _ = handle.await;
        assert!(
            !completed.load(Ordering::SeqCst),
            "cancelled task must not reach completion"
        );
    }

    #[tokio::test]
    async fn script_canceller_trips_the_stop_flag() {
        let reg = CancelRegistry::default();
        let session = SessionId::new("s1");
        let flag = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        reg.register(
            &session,
            TaskCanceller::script(handle.abort_handle(), flag.clone()),
        );

        reg.cancel_session(&session);
        assert!(
            flag.load(Ordering::SeqCst),
            "a script canceller must trip the engine's stop flag"
        );
        handle.abort();
    }

    #[test]
    fn cancel_unknown_session_is_a_noop() {
        let reg = CancelRegistry::default();
        reg.cancel_session(&SessionId::new("nope"));
    }
}
