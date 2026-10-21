//! One session's seq counter, minted and broadcast under one lock (#893,
//! ADR-0244).

use std::sync::{Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::protocol::OutEvent;

/// A session's monotonic seq counter (ADR-0068). Every seq-bearing event for
/// the session is minted *and sent* inside [`with_next`][Self::with_next]'s
/// critical section, so two emitters for one session (the session task and a
/// runtime service, or two runtime tasks) can't deliver seq 6 before seq 5:
/// broadcast order is seq order, and a head's `seq > last_seen` dedupe folds
/// every event. The lock is per session, so sessions never wait on each
/// other, and the broadcast `send` never blocks.
///
/// The session task and the engine's seq registry share one cell per id, and
/// the registry hands the same cell back on a resume, so the counter outlives
/// the task (ADR-0239, ADR-0244).
#[derive(Debug, Default)]
pub struct SeqCell(Mutex<u64>);

impl SeqCell {
    /// A counter whose last minted seq is `seq` (`0`: none yet).
    pub fn new(seq: u64) -> Self {
        Self(Mutex::new(seq))
    }

    // A poisoned lock is recovered: the critical section only bumps a `u64`
    // and sends, so a panic in an event builder can't leave it half-written.
    fn lock(&self) -> MutexGuard<'_, u64> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The last seq minted (`0` before the first).
    pub fn current(&self) -> u64 {
        *self.lock()
    }

    /// Lift the counter to at least `seq`; never lowers it.
    pub(crate) fn raise(&self, seq: u64) {
        let mut cur = self.lock();
        *cur = (*cur).max(seq);
    }

    /// Mint the next seq and run `f` with it while the lock is held. `f` must
    /// send its event (and do nothing that blocks or re-enters this cell), so
    /// the event leaves in seq order.
    pub(crate) fn with_next<R>(&self, f: impl FnOnce(u64) -> R) -> R {
        let mut cur = self.lock();
        *cur += 1;
        f(*cur)
    }

    /// Mint the next seq, build the event from it and broadcast it, in one
    /// critical section.
    pub(crate) fn emit(
        &self,
        events: &broadcast::Sender<OutEvent>,
        make: impl FnOnce(u64) -> OutEvent,
    ) {
        self.with_next(|seq| {
            let _ = events.send(make(seq));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionId;

    #[test]
    fn mints_above_the_seed_and_raise_never_lowers() {
        let cell = SeqCell::new(4);
        assert_eq!(cell.with_next(|s| s), 5);
        cell.raise(3);
        assert_eq!(cell.current(), 5);
        cell.raise(9);
        assert_eq!(cell.with_next(|s| s), 10);
    }

    /// Many threads emitting for one session: the broadcast receives every
    /// event in seq order (#893).
    #[test]
    fn concurrent_emits_are_delivered_in_seq_order() {
        let cell = std::sync::Arc::new(SeqCell::default());
        let (tx, mut rx) = broadcast::channel(8 * 500);
        let sid = SessionId::new("s");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let (cell, tx, sid) = (cell.clone(), tx.clone(), sid.clone());
                scope.spawn(move || {
                    for _ in 0..500 {
                        cell.emit(&tx, |seq| OutEvent::Done {
                            session: sid.clone(),
                            seq,
                        });
                    }
                });
            }
        });
        let mut last = 0;
        while let Ok(ev) = rx.try_recv() {
            let seq = ev.seq().expect("Done carries a seq");
            assert_eq!(seq, last + 1, "delivered out of order");
            last = seq;
        }
        assert_eq!(last, 8 * 500);
    }
}
