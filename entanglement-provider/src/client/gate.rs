//! One limiter key's live state (#918): a sliding window over requests and
//! tokens, and a resizable in-flight cap. A key keeps one [`Gate`] for its
//! whole life; changed limits are applied to it in place, so a caller with
//! different limits never resets the window or hands out a fresh set of
//! permits while the old ones are still in flight.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::sleep;

use super::endpoint::WaiterGuard;
use super::limiter::RateLimits;

/// The sliding window `rpm`/`tpm` count over.
pub(super) const WINDOW: Duration = Duration::from_secs(60);

/// The semaphore size standing for "no `concurrency`", so a cap can move
/// between set and unset in place.
const UNLIMITED: usize = Semaphore::MAX_PERMITS;

/// How long until a request costing `tokens` fits in the window `sent`
/// (`(admitted at, tokens)` pairs, oldest first), or `None` if it fits now.
/// Prunes entries older than [`WINDOW`] as a side effect. A single request
/// larger than the whole `tpm` still goes once the window is empty —
/// otherwise it could never be sent.
pub(super) fn window_wait(
    sent: &mut VecDeque<(Instant, u32)>,
    limits: &RateLimits,
    now: Instant,
    tokens: u32,
) -> Option<Duration> {
    while sent
        .front()
        .is_some_and(|(at, _)| now.duration_since(*at) >= WINDOW)
    {
        sent.pop_front();
    }
    let requests_full = limits
        .rpm
        .is_some_and(|rpm| sent.len() >= rpm.max(1) as usize);
    let used: u64 = sent.iter().map(|(_, t)| u64::from(*t)).sum();
    let tokens_full = limits
        .tpm
        .is_some_and(|tpm| !sent.is_empty() && used + u64::from(tokens) > u64::from(tpm.max(1)));
    if !(requests_full || tokens_full) {
        return None;
    }
    // The oldest entry expiring is the earliest the window can change.
    sent.front()
        .map(|(at, _)| (*at + WINDOW).saturating_duration_since(now))
}

fn permits(limits: &RateLimits) -> usize {
    limits
        .concurrency
        .map_or(UNLIMITED, |cap| cap.clamp(1, UNLIMITED))
}

struct Config {
    limits: RateLimits,
    /// Permits a shrink could not take back because they were in flight;
    /// each is forgotten instead of returned when its holder finishes.
    debt: usize,
}

/// One key's live limits: the window and the in-flight semaphore.
pub(super) struct Gate {
    config: Mutex<Config>,
    sent: Mutex<VecDeque<(Instant, u32)>>,
    /// Pacers queue here so the window admits them in arrival order (tokio's
    /// mutex is FIFO): without it a large request could be overtaken forever
    /// by smaller ones that keep fitting.
    queue: tokio::sync::Mutex<()>,
    /// Pacers currently sleeping on a full window — display only.
    blocked: AtomicUsize,
    semaphore: Arc<Semaphore>,
}

impl Gate {
    pub(super) fn new(limits: RateLimits) -> Self {
        Self {
            config: Mutex::new(Config { limits, debt: 0 }),
            sent: Mutex::new(VecDeque::new()),
            queue: tokio::sync::Mutex::new(()),
            blocked: AtomicUsize::new(0),
            semaphore: Arc::new(Semaphore::new(permits(&limits))),
        }
    }

    pub(super) fn limits(&self) -> RateLimits {
        self.config.lock().expect("gate config poisoned").limits
    }

    /// Apply `limits` in place — the window keeps its entries and the cap is
    /// resized, never rebuilt. Returns whether anything changed.
    pub(super) fn update(&self, limits: RateLimits) -> bool {
        let mut config = self.config.lock().expect("gate config poisoned");
        if config.limits == limits {
            return false;
        }
        let (old, new) = (permits(&config.limits), permits(&limits));
        if new > old {
            let grow = new - old;
            let repaid = grow.min(config.debt);
            config.debt -= repaid;
            self.semaphore.add_permits(grow - repaid);
        } else {
            let shrink = old - new;
            config.debt += shrink - self.semaphore.forget_permits(shrink);
        }
        config.limits = limits;
        true
    }

    /// The configured in-flight cap, `None` when this key has none.
    pub(super) fn cap(&self) -> Option<usize> {
        self.limits().concurrency.map(|cap| cap.max(1))
    }

    /// Requests currently holding one of this key's permits.
    pub(super) fn in_flight(&self) -> usize {
        let config = self.config.lock().expect("gate config poisoned");
        (permits(&config.limits) + config.debt).saturating_sub(self.semaphore.available_permits())
    }

    /// Time until the window admits the next request, `None` when it has
    /// room. A pacer blocked on `tpm` counts too, though the window would
    /// still take a zero-token request.
    pub(super) fn window_full_for(&self) -> Option<Duration> {
        let limits = self.limits();
        let mut sent = self.sent.lock().expect("limiter window poisoned");
        let now = Instant::now();
        window_wait(&mut sent, &limits, now, 0).or_else(|| {
            if self.blocked.load(Ordering::Relaxed) == 0 {
                return None;
            }
            sent.front()
                .map(|(at, _)| (*at + WINDOW).saturating_duration_since(now))
        })
    }

    /// Wait for a free in-flight slot.
    pub(super) async fn acquire(self: &Arc<Self>) -> Permit {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("limiter semaphore is never closed");
        Permit {
            permit: Some(permit),
            gate: self.clone(),
        }
    }

    /// Wait (in arrival order) until the window has room for `tokens`, then
    /// record the request. Returns the recorded entry's instant for
    /// [`unrecord`][Self::unrecord], `None` when this key has no window.
    pub(super) async fn pace(&self, tokens: u32) -> Option<Instant> {
        let _turn = self.queue.lock().await;
        let mut blocked = None;
        loop {
            let limits = self.limits();
            if limits.rpm.is_none() && limits.tpm.is_none() {
                return None;
            }
            let wait = {
                let mut sent = self.sent.lock().expect("limiter window poisoned");
                let now = Instant::now();
                match window_wait(&mut sent, &limits, now, tokens) {
                    None => {
                        sent.push_back((now, tokens));
                        return Some(now);
                    }
                    Some(wait) => wait,
                }
            };
            blocked.get_or_insert_with(|| WaiterGuard::new(&self.blocked));
            sleep(wait).await;
        }
    }

    /// Give back a recorded request that was never sent.
    pub(super) fn unrecord(&self, at: Instant, tokens: u32) {
        let mut sent = self.sent.lock().expect("limiter window poisoned");
        if let Some(i) = sent.iter().position(|entry| *entry == (at, tokens)) {
            sent.remove(i);
        }
    }
}

/// One in-flight slot on a [`Gate`]. On drop it returns to the semaphore —
/// or is forgotten while the gate still owes permits from a shrink.
pub(super) struct Permit {
    permit: Option<OwnedSemaphorePermit>,
    gate: Arc<Gate>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        // Never panic in drop; the config stays consistent under poison
        // because every writer finishes its update before releasing.
        let mut config = self
            .gate
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(permit) = self.permit.take() {
            if config.debt > 0 {
                config.debt -= 1;
                permit.forget();
            }
        }
    }
}
