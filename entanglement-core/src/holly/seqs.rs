//! The shared per-session seq registry (#157, ADR-0068) and the retired
//! high-water it keeps after a session's task is gone (#884, ADR-0239).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::protocol::SessionId;

/// How many retired sessions keep a high-water entry. Past it the entry
/// touched longest ago is evicted (ADR-0239): a runtime emit for that id then
/// falls back to the seq-`0` sentinel, and a resume still seeds from its log.
/// Exact reclamation (a runtime-held lease instead of a cap) is #891.
pub(crate) const RETIRED_CAP: usize = 4096;

/// Per-session monotonic seq counters, shared between each session task (which
/// owns the same `Arc<AtomicU64>` as its `Session::seq`) and the supervisor +
/// runtime (#157). A runtime service authoring an event for a *parked* session —
/// an approval `ToolRequest`, a `Plan`/`TaskList` snapshot, a `FileChange` —
/// mints a fresh seq via [`Holly::emit_for_session`][super::Holly::emit_for_session]
/// instead of reusing the parked `ToolExec` seq, so `(session, seq)` stays unique
/// across every authored event.
///
/// A session task registers its counter on start and retires it on exit. The
/// retired session's last seq stays behind as a high-water mark (#884): a
/// detached script's card (ADR-0237) parked after a hibernate, an end or a
/// close still mints a seq above every one already used, and a resume seeds
/// its counter above that mark, so no seq is ever reused. One `Mutex` (never
/// held across an `.await`) orders every read and write.
#[derive(Clone, Default)]
pub(crate) struct SeqRegistry(Arc<Mutex<Seqs>>);

#[derive(Default)]
struct Seqs {
    live: HashMap<SessionId, Arc<AtomicU64>>,
    retired: HashMap<SessionId, Retired>,
    /// Logical clock for the retired map's least-recently-touched eviction.
    tick: u64,
}

struct Retired {
    high: u64,
    touched: u64,
}

impl SeqRegistry {
    // A poisoned lock is recovered, not propagated: every critical section is
    // a single map insert/remove or an atomic bump, so a panic elsewhere can't
    // leave the maps half-updated.
    fn lock(&self) -> MutexGuard<'_, Seqs> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publish `session`'s counter as live. A retired high-water for the id
    /// (seqs minted while it was hibernated) raises the counter first, so the
    /// resumed sequence continues above them even if the log never got them.
    pub(crate) fn register(&self, session: &SessionId, counter: &Arc<AtomicU64>) {
        let mut seqs = self.lock();
        if let Some(r) = seqs.retired.remove(session) {
            counter.fetch_max(r.high, Ordering::Relaxed);
        }
        // A task still tearing down under this id (a resume racing its
        // hibernate) may hold a live entry: continue above its counter too.
        if let Some(old) = seqs.live.insert(session.clone(), Arc::clone(counter)) {
            counter.fetch_max(old.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }

    /// Take `counter` out of the live set, keeping its value as the retired
    /// high-water. When another task has already registered under `session`
    /// (a resume overtook this task's teardown), that task stays live and only
    /// learns this counter's value, so neither its liveness nor its sequence
    /// is clobbered (ADR-0239 §7).
    pub(crate) fn retire(&self, session: &SessionId, counter: &Arc<AtomicU64>) {
        let mut seqs = self.lock();
        let high = counter.load(Ordering::Relaxed);
        match seqs.live.get(session) {
            Some(live) if Arc::ptr_eq(live, counter) => {
                seqs.live.remove(session);
                seqs.touch(session.clone(), high);
            }
            Some(live) => {
                live.fetch_max(high, Ordering::Relaxed);
            }
            None => {}
        }
    }

    /// Mint the next seq for `session`: from its live counter, else from its
    /// retired high-water. An id with neither (never started, or evicted past
    /// [`RETIRED_CAP`]) gets `0` and no entry — a value no counter mints, so it
    /// can't collide with an authored seq, and an unknown id can't grow the map.
    pub(crate) fn next(&self, session: &SessionId) -> u64 {
        let mut seqs = self.lock();
        if let Some(c) = seqs.live.get(session) {
            return c.fetch_add(1, Ordering::Relaxed) + 1;
        }
        match seqs.retired.get(session) {
            Some(r) => {
                let high = r.high + 1;
                seqs.touch(session.clone(), high);
                high
            }
            None => 0,
        }
    }

    /// Whether `session` has a running task. A retired high-water does not
    /// count: it outlives the task only so late emits keep a real seq.
    pub(crate) fn is_live(&self, session: &SessionId) -> bool {
        self.lock().live.contains_key(session)
    }
}

impl Seqs {
    fn touch(&mut self, session: SessionId, high: u64) {
        self.tick += 1;
        let touched = self.tick;
        self.retired.insert(session, Retired { high, touched });
        if self.retired.len() > RETIRED_CAP {
            let oldest = self
                .retired
                .iter()
                .min_by_key(|(_, r)| r.touched)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.retired.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::new(s)
    }

    #[test]
    fn a_retired_session_keeps_minting_above_its_last_seq() {
        let reg = SeqRegistry::default();
        let c = Arc::new(AtomicU64::new(0));
        reg.register(&sid("a"), &c);
        assert_eq!((reg.next(&sid("a")), reg.next(&sid("a"))), (1, 2));
        reg.retire(&sid("a"), &c);
        assert!(!reg.is_live(&sid("a")));
        assert_eq!((reg.next(&sid("a")), reg.next(&sid("a"))), (3, 4));
    }

    #[test]
    fn a_resume_continues_above_seqs_minted_while_retired() {
        let reg = SeqRegistry::default();
        let first = Arc::new(AtomicU64::new(5));
        reg.register(&sid("a"), &first);
        reg.retire(&sid("a"), &first);
        assert_eq!(reg.next(&sid("a")), 6);
        // The log only knew seq 5: the resumed counter is replay-seeded there.
        let resumed = Arc::new(AtomicU64::new(5));
        reg.register(&sid("a"), &resumed);
        assert!(reg.is_live(&sid("a")));
        assert!(
            !reg.lock().retired.contains_key(&sid("a")),
            "a resume takes the id out of the retired map"
        );
        assert_eq!(reg.next(&sid("a")), 7);
        // A log ahead of the high-water wins too.
        reg.retire(&sid("a"), &resumed);
        let ahead = Arc::new(AtomicU64::new(20));
        reg.register(&sid("a"), &ahead);
        assert_eq!(reg.next(&sid("a")), 21);
    }

    /// A resume that registers task B before the old task A retires (the
    /// supervisor sends `Hibernate` without waiting) keeps B live, and no seq
    /// repeats across A's tail, B and runtime emits.
    #[test]
    fn a_late_retire_of_the_old_task_leaves_the_resumed_one_live() {
        let reg = SeqRegistry::default();
        let a = Arc::new(AtomicU64::new(0));
        reg.register(&sid("s"), &a);
        let mut seen = vec![reg.next(&sid("s"))];
        // A's own last mint before its teardown.
        seen.push(a.fetch_add(1, Ordering::Relaxed) + 1);

        // B resumes from a log that only reached seq 1.
        let b = Arc::new(AtomicU64::new(1));
        reg.register(&sid("s"), &b);
        reg.retire(&sid("s"), &a);
        assert!(reg.is_live(&sid("s")), "A's retire must not unregister B");
        assert!(!reg.lock().retired.contains_key(&sid("s")));

        seen.push(reg.next(&sid("s")));
        seen.push(b.fetch_add(1, Ordering::Relaxed) + 1);
        seen.push(reg.next(&sid("s")));
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "no seq repeats: {seen:?}");
        assert!(seen.windows(2).all(|w| w[1] > w[0]), "monotonic: {seen:?}");
    }

    /// The other order: A retires with a value B's replay never saw, after B
    /// registered — B still continues above it.
    #[test]
    fn a_late_retire_raises_the_resumed_counter() {
        let reg = SeqRegistry::default();
        let a = Arc::new(AtomicU64::new(5));
        reg.register(&sid("s"), &a);
        let b = Arc::new(AtomicU64::new(2));
        reg.register(&sid("s"), &b);
        // Registering over A's live entry already continues above it.
        assert_eq!(reg.next(&sid("s")), 6);
        a.store(9, Ordering::Relaxed);
        reg.retire(&sid("s"), &a);
        assert_eq!(reg.next(&sid("s")), 10);
    }

    #[test]
    fn an_unknown_id_mints_zero_and_leaves_no_entry() {
        let reg = SeqRegistry::default();
        assert_eq!(reg.next(&sid("ghost")), 0);
        assert_eq!(reg.next(&sid("ghost")), 0);
        assert!(reg.lock().retired.is_empty());
        assert!(!reg.is_live(&sid("ghost")));
    }

    #[test]
    fn the_retired_map_is_bounded_and_evicts_the_least_recently_touched() {
        let reg = SeqRegistry::default();
        let retire = |id: &SessionId| {
            let c = Arc::new(AtomicU64::new(1));
            reg.register(id, &c);
            reg.retire(id, &c);
        };
        retire(&sid("s0"));
        retire(&sid("s1"));
        // An emit touches s0, so s1 is now the one touched longest ago.
        reg.next(&sid("s0"));
        for i in 2..=RETIRED_CAP {
            retire(&sid(&format!("s{i}")));
        }
        let seqs = reg.lock();
        assert_eq!(seqs.retired.len(), RETIRED_CAP);
        assert!(seqs.retired.contains_key(&sid("s0")), "touched, so kept");
        assert!(!seqs.retired.contains_key(&sid("s1")), "oldest, so evicted");
    }
}
