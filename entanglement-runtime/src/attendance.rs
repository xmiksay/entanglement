//! Who is watching a session (ADR-0247): "unattended" is a fact about the
//! session, not only its mode. A session is **attended** while a head that
//! can answer a card watches it or any of its ancestors — the TUI watches
//! every session of its engine, a `serve` connection the sessions it owns;
//! `run` and `pipe` never watch.
//!
//! A head registers through a [`Watcher`] handle, which unregisters on drop.
//! The gate asks [`Attendance::attends_any`] with the call's ancestor chain
//! (the spawn guard's parent walk), and a parked card or question waits on
//! an [`Expiry`]: no deadline when attended, else the mode's
//! `question_timeout` — waived for good the moment a watcher arrives. The
//! waiver is kept per `(session, request_id)` until the card is answered or
//! its session ends, so a card a hibernate unwound is re-offered on resume
//! still waiting, watched or not. It lives in memory: a process restart
//! forgets it (the restarted TUI watches every session anyway).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use entanglement_core::SessionId;
use tokio::sync::watch;

/// The engine's attendance registry. Cheap to clone; every clone is the
/// same registry. An embedder that registers no watcher keeps today's
/// mode-only posture: every session is unattended.
#[derive(Clone)]
pub struct Attendance {
    inner: Arc<Inner>,
}

struct Inner {
    watchers: Mutex<Watchers>,
    /// Bumped whenever a watcher starts watching something, so a parked
    /// timer can re-check whether its session became attended.
    arrived: watch::Sender<u64>,
}

/// One parked card or question: its session and request id.
type Park = (SessionId, String);

#[derive(Default)]
struct Watchers {
    next: u64,
    by_id: HashMap<u64, Watched>,
    /// Parks whose deadline was waived and that are not answered yet.
    waived: HashSet<Park>,
}

enum Watched {
    All,
    Sessions(HashSet<SessionId>),
}

impl Default for Attendance {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                watchers: Mutex::default(),
                arrived: watch::channel(0).0,
            }),
        }
    }
}

impl Attendance {
    pub fn new() -> Self {
        Self::default()
    }

    /// A watcher of every session — the TUI, which shows each session its
    /// engine runs.
    pub fn watch_all(&self) -> Watcher {
        self.register(Watched::All)
    }

    /// A watcher of the sessions it [`attend`s](Watcher::attend) — a `serve`
    /// connection.
    pub fn watcher(&self) -> Watcher {
        self.register(Watched::Sessions(HashSet::new()))
    }

    /// Run `head` while watching every session — the TUI's whole run.
    pub async fn watching_all<F: std::future::Future>(&self, head: F) -> F::Output {
        let _watching = self.watch_all();
        head.await
    }

    /// Forget the waivers of a session that ended (not one that hibernated:
    /// its cards are re-offered on resume).
    pub fn forget_session(&self, session: &SessionId) {
        self.lock().waived.retain(|(s, _)| s != session);
    }

    fn waive(&self, park: &Park) {
        self.lock().waived.insert(park.clone());
    }

    fn register(&self, watched: Watched) -> Watcher {
        let all = matches!(watched, Watched::All);
        let id = {
            let mut guard = self.lock();
            let id = guard.next;
            guard.next += 1;
            guard.by_id.insert(id, watched);
            id
        };
        if all {
            self.arrived();
        }
        Watcher {
            id,
            attendance: self.clone(),
        }
    }

    /// Whether any session of `chain` — a call's session and its ancestors
    /// — has a watcher.
    pub fn attends_any(&self, chain: &[SessionId]) -> bool {
        self.lock().by_id.values().any(|watched| match watched {
            Watched::All => true,
            Watched::Sessions(set) => chain.iter().any(|s| set.contains(s)),
        })
    }

    /// Resolves once `chain` is attended (at once if it already is).
    async fn until_attended(&self, chain: &[SessionId]) {
        // Subscribed before the check, so an arrival between the two still
        // wakes this wait.
        let mut arrivals = self.inner.arrived.subscribe();
        while !self.attends_any(chain) {
            if arrivals.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    fn arrived(&self) {
        self.inner.arrived.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn lock(&self) -> MutexGuard<'_, Watchers> {
        self.inner
            .watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// One head's registration. Dropping it stops the watch; a card already
/// waiting with no deadline keeps waiting (ADR-0247).
pub struct Watcher {
    id: u64,
    attendance: Attendance,
}

impl Watcher {
    /// Watch `session` (and so every session it spawns).
    pub fn attend(&self, session: &SessionId) {
        let added = match self.attendance.lock().by_id.get_mut(&self.id) {
            Some(Watched::Sessions(set)) => set.insert(session.clone()),
            _ => false,
        };
        if added {
            self.attendance.arrived();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.attendance.lock().by_id.remove(&self.id);
    }
}

/// When a parked card or question stops waiting (ADR-0247). Clones share
/// the waiver, so an `ask_user` re-parked by a replace keeps waiting once a
/// watcher arrived during an earlier pass.
#[derive(Clone)]
pub struct Expiry {
    after: Option<Duration>,
    watch: Option<Watch>,
    waived: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Watch {
    attendance: Attendance,
    chain: Vec<SessionId>,
    park: Park,
}

impl Expiry {
    /// Wait for an answer, however long it takes.
    pub fn never() -> Self {
        Self::after(None)
    }

    /// A plain deadline (`None`: none), whoever watches.
    pub fn after(timeout: Option<Duration>) -> Self {
        Self {
            after: timeout,
            watch: None,
            waived: Arc::default(),
        }
    }

    /// `timeout` for the park `request_id` of `chain[0]` while nobody
    /// watches `chain` (the session and its ancestors). Waived — no
    /// deadline — when someone does, when this park was waived before (a
    /// re-offer after a hibernate), or the moment a watcher arrives.
    pub fn unless_attended(
        timeout: Option<Duration>,
        attendance: &Attendance,
        chain: &[SessionId],
        request_id: &str,
    ) -> Self {
        if timeout.is_none() {
            return Self::never();
        }
        let watch = chain.first().map(|session| Watch {
            attendance: attendance.clone(),
            chain: chain.to_vec(),
            park: (session.clone(), request_id.to_string()),
        });
        let waived = watch.as_ref().is_some_and(|w| {
            let held = attendance.lock().waived.contains(&w.park);
            held || attendance.attends_any(chain)
        });
        if waived {
            if let Some(w) = &watch {
                attendance.waive(&w.park);
            }
        }
        Self {
            after: timeout.filter(|_| !waived),
            watch,
            waived: Arc::new(AtomicBool::new(waived)),
        }
    }

    /// The deadline in seconds, `0` for none — for a refusal's text.
    pub fn secs(&self) -> u64 {
        self.after.map(|d| d.as_secs()).unwrap_or_default()
    }

    /// The park was answered: its waiver is spent, so a later park reusing
    /// the id starts fresh.
    pub(crate) fn settle(&self) {
        if let Some(w) = &self.watch {
            w.attendance.lock().waived.remove(&w.park);
        }
    }

    /// Resolves when the wait is over: the deadline passed with nobody
    /// watching. Never resolves without a deadline or once waived.
    pub async fn elapsed(self) {
        let Some(after) = self.after.filter(|_| !self.waived.load(Ordering::SeqCst)) else {
            return std::future::pending().await;
        };
        let Some(w) = &self.watch else {
            return tokio::time::sleep(after).await;
        };
        // Biased toward attendance, and the deadline re-checks it: a watcher
        // registered just before its bump was seen still wins (ADR-0247 §3).
        tokio::select! {
            biased;
            () = w.attendance.until_attended(&w.chain) => {}
            () = tokio::time::sleep(after) => {
                if !w.attendance.attends_any(&w.chain) {
                    return;
                }
            }
        }
        self.waived.store(true, Ordering::SeqCst);
        w.attendance.waive(&w.park);
        std::future::pending::<()>().await
    }
}

#[cfg(test)]
#[path = "attendance_tests.rs"]
mod tests;
