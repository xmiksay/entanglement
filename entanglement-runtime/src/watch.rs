//! The debounced file-watch primitive (#329): one callback per settled burst
//! of filesystem events under a set of paths. The library reloads nothing
//! itself — `plan_watch` uses this for the plans folder, and an embedder
//! (`skutter`'s definitions watcher) for whatever it loads from disk.
//!
//! Only paths that exist at spawn are watched (`notify` can't watch a path
//! that isn't there yet).

use std::path::PathBuf;
use std::time::Duration;

use notify_debouncer_mini::DebounceEventResult;

/// Pure watch/debounce primitive: watches every existing path in `paths`
/// (non-existent ones are skipped — see the module doc) and calls `on_change`
/// once per debounced batch of filesystem events, however many raw events or
/// files were touched within the `debounce` window. Independently
/// unit-testable without touching any real definition loader. Returns `None`
/// (spawning nothing) when no candidate path exists.
///
/// Shared by `plan_watch`'s plans-folder watch (#627) and an embedder's own
/// watches (`skutter`'s definitions watcher).
pub fn spawn_debounced_watcher(
    paths: Vec<PathBuf>,
    debounce: Duration,
    mut on_change: impl FnMut() + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let existing: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
    if existing.is_empty() {
        return None;
    }

    // `notify_debouncer_mini::new_debouncer` already runs its own background
    // `std::thread` (notify's watch API is blocking) and batches every raw
    // event that lands inside one `debounce` window into a single
    // `handle_event` call — the debounce-collapses-a-burst behavior the
    // watcher unit tests exercise. The handler forwards one signal per batch
    // over an unbounded channel; the tokio task below just relays it into
    // `on_change`, keeping the `Debouncer` guard alive (it stops on drop) for
    // as long as the task lives.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut debouncer =
        match notify_debouncer_mini::new_debouncer(debounce, move |res: DebounceEventResult| {
            match res {
                // `notify_debouncer_mini` can emit a provisional
                // `AnyContinuous` event for a path ahead of its final `Any`
                // — the same logical write straddling a debounce tick, not
                // a second change (see `DebounceDataInner::debounced_events`:
                // an entry still updating right at the deadline is reported
                // once as `AnyContinuous` and rescheduled, only settling to
                // `Any` on a later tick with no further updates). Counting
                // `AnyContinuous` double-fired the reload callback for one
                // edit; only a batch containing a real `Any` means "this
                // change has settled".
                Ok(events)
                    if events
                        .iter()
                        .any(|e| e.kind == notify_debouncer_mini::DebouncedEventKind::Any) =>
                {
                    tracing::debug!(count = events.len(), "watcher: debounced batch");
                    let _ = tx.send(());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("watcher error: {e:#}"),
            }
        }) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("could not start a file watcher: {e:#}");
                return None;
            }
        };
    for p in &existing {
        if let Err(e) = debouncer
            .watcher()
            .watch(p, notify::RecursiveMode::Recursive)
        {
            tracing::warn!("could not watch {}: {e:#}", p.display());
        }
    }

    Some(tokio::spawn(async move {
        let _debouncer = debouncer; // held for the task's lifetime; stops on drop
        while rx.recv().await.is_some() {
            on_change();
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A debounce window generous enough that five back-to-back synchronous
    /// `fs::write` calls (each well under a millisecond in isolation) stay
    /// inside one window even under a heavily loaded/parallel `cargo test`
    /// run, where scheduling jitter can stretch "instant" syscalls across tens
    /// of milliseconds. `notify_debouncer_mini` emits a provisional
    /// `AnyContinuous` (in addition to the final `Any`) for a burst whose
    /// *total* span approaches the debounce window — a real flake source at a
    /// too-tight window, not a bug in the watcher.
    const TEST_DEBOUNCE: Duration = Duration::from_millis(1000);

    #[tokio::test]
    async fn one_change_fires_the_callback_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.md");
        std::fs::write(&file, "v1").unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let counted = count.clone();
        let handle =
            spawn_debounced_watcher(vec![dir.path().to_path_buf()], TEST_DEBOUNCE, move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })
            .expect("existing dir must spawn a watcher");

        std::fs::write(&file, "v2").unwrap();
        tokio::time::sleep(TEST_DEBOUNCE * 3).await;

        assert_eq!(count.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn a_burst_of_writes_collapses_into_one_callback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.md");
        std::fs::write(&file, "v0").unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let counted = count.clone();
        let handle =
            spawn_debounced_watcher(vec![dir.path().to_path_buf()], TEST_DEBOUNCE, move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })
            .expect("existing dir must spawn a watcher");

        // Five rapid writes, all well within the debounce window — no sleeps
        // between them — must collapse into (at most a couple of) reload
        // callbacks, not five.
        for i in 0..5 {
            std::fs::write(&file, format!("v{i}")).unwrap();
        }
        tokio::time::sleep(TEST_DEBOUNCE * 3).await;

        // `notify_debouncer_mini` can emit a provisional `AnyContinuous`
        // event ahead of the final `Any` for the same write — the debounced
        // watcher now only counts a batch as a real change when it contains
        // an `Any` (see `spawn_debounced_watcher`'s handler), so a burst
        // collapses to exactly 1 callback, not "1 or 2 depending on
        // scheduler jitter".
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "a burst within the debounce window must collapse to exactly 1 callback"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn missing_path_spawns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(spawn_debounced_watcher(vec![missing], TEST_DEBOUNCE, || {}).is_none());
    }

    #[tokio::test]
    async fn empty_paths_spawns_nothing() {
        assert!(spawn_debounced_watcher(Vec::new(), TEST_DEBOUNCE, || {}).is_none());
    }
}
