//! The shared per-session seq registry (#157, ADR-0068), the retired seq
//! cell it keeps after a session's task is gone (#884, ADR-0239), and
//! the leases detached work pins it with (#891, ADR-0244).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::seq_cell::SeqCell;
use crate::protocol::SessionId;

/// How many retired sessions keep their counter while nothing outside the
/// registry holds it. Past it the unpinned entry touched longest ago is
/// evicted (ADR-0239 §6, ADR-0244): an emit for that id then falls back to
/// the seq-`0` sentinel, and a resume seeds from its log. A pinned entry (a
/// [`SeqLease`], an emit in flight, a task still tearing down) is never
/// evicted, so the map grows past the cap when every entry is pinned.
pub(crate) const RETIRED_CAP: usize = 4096;

/// Per-session seq counters, shared between each session task (which owns the
/// same [`SeqCell`] as its `Session::seq`) and the supervisor + runtime (#157).
/// A runtime service authoring an event for a session — an approval
/// `ToolRequest`, a `Plan`/`TaskList` snapshot, a `FileChange` — mints through
/// [`Holly::emit_for_session`][super::Holly::emit_for_session] from the same
/// cell, so `(session, seq)` stays unique and delivery order is seq order
/// (#893).
///
/// A session task registers on start and retires on exit. The retired cell
/// stays behind (#884): a detached script's card (ADR-0237) parked after a
/// hibernate, an end or a close still mints above every seq already used, and
/// a resume gets the same cell back, so no seq is ever reused. The registry
/// `Mutex` is never held across an `.await`, and never while a cell is locked
/// by an emit: an emit clones the cell out, unlocks, then locks the cell.
#[derive(Clone, Default)]
pub(crate) struct SeqRegistry(Arc<Mutex<Seqs>>);

#[derive(Default)]
struct Seqs {
    live: HashMap<SessionId, Arc<SeqCell>>,
    retired: HashMap<SessionId, Retired>,
    /// Logical clock for the retired map's least-recently-touched eviction.
    tick: u64,
}

struct Retired {
    cell: Arc<SeqCell>,
    touched: u64,
}

/// A pin on one session's seq counter (#891, ADR-0244). While any lease for
/// an id lives, the registry never evicts that id's retired counter, so
/// detached work that outlives its session (a background `rhai` script)
/// keeps minting real seqs however many other sessions retire meanwhile. The
/// runtime takes one when the work starts and drops it when the work ends;
/// dropping is the release (RAII). A lease of an id that never started pins
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct SeqLease(Option<Arc<SeqCell>>);

impl SeqLease {
    /// Whether this lease pins a counter (`false` for an id that never
    /// started).
    pub fn is_pinning(&self) -> bool {
        self.0.is_some()
    }
}

impl SeqRegistry {
    // A poisoned lock is recovered, not propagated: every critical section is
    // a few map inserts/removes, so a panic elsewhere can't leave the maps
    // half-updated.
    fn lock(&self) -> MutexGuard<'_, Seqs> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publish `session` as live and return the counter its task must emit
    /// through: the id's retired cell when it has one (seqs minted while it
    /// was hibernated stay below the resumed sequence even if the log never
    /// got them), else a fresh one. Either way it is raised to `seed`, the
    /// replay's highest seq. A task waits for the id's old task to exit
    /// before it registers (ADR-0244), and an exiting task retires first, so
    /// a live entry is left only by a task that panicked; the new task then
    /// shares its cell rather than restarting the sequence.
    pub(crate) fn register(&self, session: &SessionId, seed: u64) -> Arc<SeqCell> {
        let mut seqs = self.lock();
        let cell = match seqs.retired.remove(session) {
            Some(r) => r.cell,
            None => seqs.live.get(session).cloned().unwrap_or_default(),
        };
        cell.raise(seed);
        seqs.live.insert(session.clone(), Arc::clone(&cell));
        cell
    }

    /// Move `session` from live to retired, keeping its cell (ADR-0239).
    pub(crate) fn retire(&self, session: &SessionId) {
        let mut seqs = self.lock();
        if let Some(cell) = seqs.live.remove(session) {
            seqs.touch(session.clone(), cell);
        }
    }

    /// Run `f` with `session`'s next seq inside its cell's critical section
    /// (#893): from its live cell, else its retired one. An id with neither
    /// (never started, or evicted past [`RETIRED_CAP`]) gets `0` and no entry
    /// — a value no counter mints, so it can't collide with an authored seq,
    /// and an unknown id can't grow the map.
    pub(crate) fn with_next<R>(&self, session: &SessionId, f: impl FnOnce(u64) -> R) -> R {
        match self.cell(session) {
            Some(cell) => cell.with_next(f),
            None => f(0),
        }
    }

    /// The id's cell, touching a retired one. The clone pins it until the
    /// caller drops it, so an emit can't race its eviction.
    fn cell(&self, session: &SessionId) -> Option<Arc<SeqCell>> {
        let mut seqs = self.lock();
        if let Some(cell) = seqs.live.get(session) {
            return Some(Arc::clone(cell));
        }
        let cell = Arc::clone(&seqs.retired.get(session)?.cell);
        seqs.touch(session.clone(), Arc::clone(&cell));
        Some(cell)
    }

    /// Pin `session`'s counter for as long as the returned lease lives.
    pub(crate) fn lease(&self, session: &SessionId) -> SeqLease {
        let seqs = self.lock();
        let cell = match seqs.live.get(session) {
            Some(cell) => Some(cell),
            None => seqs.retired.get(session).map(|r| &r.cell),
        };
        SeqLease(cell.cloned())
    }

    /// Whether `session` has a running task. A retired cell does not count:
    /// it outlives the task only so late emits keep a real seq.
    pub(crate) fn is_live(&self, session: &SessionId) -> bool {
        self.lock().live.contains_key(session)
    }
}

impl Seqs {
    fn touch(&mut self, session: SessionId, cell: Arc<SeqCell>) {
        self.tick += 1;
        let touched = self.tick;
        self.retired.insert(session, Retired { cell, touched });
        self.shrink();
    }

    /// Evict the least recently touched unpinned entries until the map is
    /// back within [`RETIRED_CAP`]. Pinned means the cell is held outside the
    /// registry: evicting it would let a resume start a second counter beside
    /// the one still in use. An overshoot left by entries pinned at the time
    /// is reclaimed by a later touch, once their holders let go. While every
    /// entry over the cap stays pinned, each touch rescans the whole map and
    /// evicts nothing: O(n) per retire or retired-id emit, with n bounded by
    /// the cap plus the live leases. Accepted: only retires and late emits
    /// touch, never a live session's emits.
    fn shrink(&mut self) {
        while self.retired.len() > RETIRED_CAP {
            let oldest = self
                .retired
                .iter()
                .filter(|(_, r)| Arc::strong_count(&r.cell) == 1)
                .min_by_key(|(_, r)| r.touched)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => self.retired.remove(&id),
                None => break,
            };
        }
    }
}

#[cfg(test)]
mod tests;
