//! The per-endpoint adaptive (AIMD) pacing gate.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::time::sleep;

use super::SLOWDOWN_CAP;

/// Client-side **adaptive pacing gate**, one per endpoint. Requests are spaced
/// at least `interval` apart (`interval = 60s / rpm` at rest). Unlike the old
/// bursty token bucket (which started full and let `rpm` callers fire at once —
/// the very overshoot that trips a provider's concurrency/RPM limit when many
/// sessions spawn together), a spawn-storm is smoothed from the *first* request.
///
/// The spacing is **adaptive (AIMD)**: each 429 slows it ([`penalize`], doubling,
/// multiplicative, capped at `SLOWDOWN_CAP × base`); each success speeds it back
/// toward the base ([`relax`], additive, one base unit at a time). So a
/// too-high default RPM self-corrects down to the endpoint's real limit and
/// recovers once the storm clears — without any per-provider tuning.
///
/// [`penalize`]: RateLimiter::penalize
/// [`relax`]: RateLimiter::relax
pub(super) struct RateLimiter {
    /// The at-rest spacing (`60s / rpm`) and the floor `relax` recovers to.
    pub(super) base: Duration,
    /// The most-throttled spacing `penalize` will grow to (`base × SLOWDOWN_CAP`).
    pub(super) max: Duration,
    pub(super) state: Mutex<PaceState>,
}

pub(super) struct PaceState {
    /// Current spacing between consecutive requests, in `[base, max]`.
    pub(super) interval: Duration,
    /// Earliest instant the next request may proceed. Each `acquire` reserves
    /// this slot and advances it, so concurrent callers get distinct, spaced
    /// slots instead of all firing at once.
    pub(super) next_slot: Instant,
}

impl RateLimiter {
    pub(super) fn new(rpm: u32) -> Self {
        let rpm = rpm.max(1);
        let base = Duration::from_millis(60_000 / rpm as u64);
        Self {
            base,
            max: base * SLOWDOWN_CAP,
            state: Mutex::new(PaceState {
                interval: base,
                next_slot: Instant::now(),
            }),
        }
    }

    /// Reserve the next pacing slot and wait until it arrives. The lock is held
    /// only to reserve the slot, never across the `await`.
    pub(super) async fn acquire(&self) {
        let slot = {
            let mut st = self.state.lock().expect("rate limiter poisoned");
            let slot = st.next_slot.max(Instant::now());
            st.next_slot = slot + st.interval;
            slot
        };
        let now = Instant::now();
        if slot > now {
            sleep(slot - now).await;
        }
    }

    /// A 429 was seen on this endpoint: double the spacing (capped at `max`), so
    /// every caller metering through this gate slows together.
    pub(super) fn penalize(&self) {
        let mut st = self.state.lock().expect("rate limiter poisoned");
        st.interval = (st.interval * 2).min(self.max);
    }

    /// A request to this endpoint succeeded: step the spacing back toward `base`
    /// by one base unit (additive increase of the effective rate). A no-op once
    /// already at `base`.
    pub(super) fn relax(&self) {
        let mut st = self.state.lock().expect("rate limiter poisoned");
        st.interval = st.interval.saturating_sub(self.base).max(self.base);
    }
}
