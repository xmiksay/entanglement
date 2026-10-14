//! At most one detached script-triggered connect per server (#831). A
//! script whose budget runs out mid-connect leaves the connect running for
//! whoever queued behind it; without this, a `try`/`catch` loop would start
//! a fresh task per iteration and pile them up.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

/// A connect's settled outcome: `Err` is the user-facing failure text.
type Settled = Option<Result<(), String>>;

/// Keyed in-flight connects; each waiter watches the one running task.
#[derive(Clone, Default)]
pub(crate) struct InFlight {
    running: Arc<Mutex<HashMap<String, watch::Receiver<Settled>>>>,
}

/// Drops `key`'s entry when its task ends — a panic included, so a dead
/// task never leaves a closed channel for later callers to wait on.
struct Settle {
    running: Arc<Mutex<HashMap<String, watch::Receiver<Settled>>>>,
    key: String,
}

impl Drop for Settle {
    fn drop(&mut self) {
        if let Ok(mut running) = self.running.lock() {
            running.remove(&self.key);
        }
    }
}

impl InFlight {
    /// Wait on `key`'s running connect, starting it from `start` only when
    /// none is. Dropping the returned future abandons the wait, never the
    /// connect.
    pub(crate) fn join<F>(
        &self,
        key: String,
        start: impl FnOnce() -> F,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static
    where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        let mut running = self
            .running
            .lock()
            .expect("in-flight connect mutex poisoned");
        if let Some(rx) = running.get(&key) {
            return wait(rx.clone());
        }
        let (tx, rx) = watch::channel(None);
        running.insert(key.clone(), rx.clone());
        // Released before `start` runs: should it panic, `Settle`'s drop
        // takes this same lock to clear the entry.
        drop(running);
        let settle = Settle {
            running: self.running.clone(),
            key,
        };
        let connect = start();
        tokio::spawn(async move {
            let _settle = settle;
            let _ = tx.send(Some(connect.await));
        });
        wait(rx)
    }
}

/// Wait for `rx`'s connect to settle.
async fn wait(mut rx: watch::Receiver<Settled>) -> Result<(), String> {
    let settled = match rx.wait_for(Option::is_some).await {
        Ok(settled) => settled.clone(),
        Err(_) => None,
    };
    settled.unwrap_or_else(|| Err("MCP connect task ended without an outcome".into()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn concurrent_joins_share_one_task_and_a_settled_key_starts_afresh() {
        let flight = InFlight::default();
        let starts = Arc::new(AtomicUsize::new(0));
        let start = |starts: Arc<AtomicUsize>| {
            move || {
                starts.fetch_add(1, Ordering::SeqCst);
                async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Err::<(), _>("down".to_string())
                }
            }
        };
        let a = flight.join("srv".into(), start(starts.clone()));
        let b = flight.join("srv".into(), start(starts.clone()));
        assert_eq!(a.await, Err("down".into()));
        assert_eq!(b.await, Err("down".into()));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        // The entry goes with the task, so the next join connects again.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let c = flight.join("srv".into(), start(starts.clone()));
        assert_eq!(c.await, Err("down".into()));
        assert_eq!(starts.load(Ordering::SeqCst), 2);
    }

    /// A panicking `start` must not deadlock on the entry's own cleanup,
    /// and leaves the key free for the next join.
    #[tokio::test]
    async fn a_panicking_start_leaves_the_key_free() {
        let flight = InFlight::default();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(flight.join(
                "srv".into(),
                || -> std::future::Ready<Result<(), String>> { panic!("start failed") },
            ));
        }));
        assert!(panicked.is_err());
        assert!(flight.running.lock().unwrap().is_empty());
        let ok = flight.join("srv".into(), || async { Ok(()) });
        assert_eq!(ok.await, Ok(()));
    }

    #[tokio::test]
    async fn an_abandoned_wait_leaves_the_connect_running() {
        let flight = InFlight::default();
        let done = Arc::new(AtomicUsize::new(0));
        let flag = done.clone();
        let wait = flight.join("srv".into(), move || async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(tokio::time::timeout(Duration::from_millis(5), wait)
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(done.load(Ordering::SeqCst), 1);
    }
}
