//! Read-only snapshot of a per-endpoint's live throttle state, for heads that
//! want to surface rate-limiting to the user (the TUI shows it only while an
//! endpoint is actually backing off). A child module of `client` so it can read
//! the otherwise-private pool/endpoint state without widening its visibility.

use std::time::{Duration, Instant};

use super::{EndpointState, HttpClient};

/// A snapshot of one endpoint's throttle posture at a moment in time. Purely
/// informational — nothing here feeds back into the request path.
#[derive(Debug, Clone)]
pub struct ThrottleStatus {
    /// The endpoint's base URL (the pool key with any API-key hash stripped),
    /// used as a compact human label.
    pub endpoint: String,
    /// Requests currently in flight against the **binding** cap (see `model`)
    /// — either this endpoint's own, or a tighter per-model cap (#521).
    pub in_flight: usize,
    /// The configured cap for whichever is binding (endpoint or model).
    pub cap: usize,
    /// Remaining time on an active `Retry-After` / 429 cool-down window, or
    /// `None` when the endpoint is not parked.
    pub backoff_remaining: Option<Duration>,
    /// Whether the adaptive pacing gate has slowed below its base rate (a 429
    /// penalized the interval), independent of an explicit cool-down window.
    pub penalized: bool,
    /// The model id whose own per-model cap is the binding (most-constrained)
    /// one, when a model's occupancy ratio is tighter than the endpoint's own
    /// (#521, ADR-0140). `None` when the endpoint-wide cap is binding — which
    /// is always the case for a model that carries no per-model cap.
    pub model: Option<String>,
}

impl EndpointState {
    /// Snapshot this endpoint's throttle posture. `endpoint` is the human label
    /// (base URL); the caller strips the pool-key hash suffix. When a
    /// per-model slot (#521) is currently *more* saturated (by occupancy
    /// ratio) than the endpoint-wide cap, its in-flight/cap and id are
    /// reported instead — the binding constraint is whichever is actually
    /// closest to blocking the next request.
    fn status(&self, endpoint: String) -> ThrottleStatus {
        let ep_in_flight = self
            .concurrency_cap
            .saturating_sub(self.concurrency.available_permits());
        let backoff_remaining = self
            .retry_after
            .lock()
            .expect("retry_after poisoned")
            .and_then(|until| until.checked_duration_since(Instant::now()))
            .filter(|d| !d.is_zero());
        let penalized = {
            let st = self.limiter.state.lock().expect("rate limiter poisoned");
            st.interval > self.limiter.base
        };

        let ep_ratio = occupancy_ratio(ep_in_flight, self.concurrency_cap);
        let tightest_model = {
            let map = self
                .model_concurrency
                .lock()
                .expect("model concurrency poisoned");
            map.iter()
                .map(|(model, slot)| {
                    let in_flight = slot.cap.saturating_sub(slot.semaphore.available_permits());
                    (model.clone(), in_flight, slot.cap)
                })
                .max_by(|a, b| {
                    occupancy_ratio(a.1, a.2)
                        .partial_cmp(&occupancy_ratio(b.1, b.2))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        };
        let (in_flight, cap, model) = match tightest_model {
            Some((model, in_flight, cap)) if occupancy_ratio(in_flight, cap) > ep_ratio => {
                (in_flight, cap, Some(model))
            }
            _ => (ep_in_flight, self.concurrency_cap, None),
        };

        ThrottleStatus {
            endpoint,
            in_flight,
            cap,
            backoff_remaining,
            penalized,
            model,
        }
    }

    /// Whether this endpoint is currently throttled: parked in a cool-down
    /// window, slowed by the pacing gate, or already at its in-flight cap.
    fn is_throttled(status: &ThrottleStatus) -> bool {
        status.backoff_remaining.is_some() || status.penalized || status.in_flight >= status.cap
    }
}

impl HttpClient {
    /// The most-throttled endpoint currently backing off — one with an active
    /// cool-down window, a penalized pacing gate, or a saturated in-flight cap —
    /// or `None` when every endpoint is at rest. Read-only over the live pool
    /// state; the TUI polls this each frame and renders an indicator only when
    /// it is `Some`.
    ///
    /// When several endpoints are throttled, the one with the longest remaining
    /// cool-down wins (else any penalized/saturated one), since that is the most
    /// user-relevant.
    pub fn throttle_status(&self) -> Option<ThrottleStatus> {
        let map = self.pool.endpoints.lock().expect("endpoint pool poisoned");
        map.iter()
            .map(|(key, state)| state.status(host_label(key)))
            .filter(EndpointState::is_throttled)
            .max_by_key(|s| s.backoff_remaining.unwrap_or_default())
    }
}

/// Fraction of `cap` currently occupied, for comparing an endpoint's own
/// saturation against a per-model slot's (#521) on a common scale. `cap == 0`
/// (never constructed in practice — `ModelSlot::new`/`EndpointState::new` both
/// floor at 1) reads as fully saturated rather than dividing by zero.
fn occupancy_ratio(in_flight: usize, cap: usize) -> f64 {
    if cap == 0 {
        return 1.0;
    }
    in_flight as f64 / cap as f64
}

/// Strip the API-key hash suffix (`"{endpoint}#{hash}"`, see `pool_key`) from a
/// pool key, leaving the base URL as a human label.
fn host_label(key: &str) -> String {
    match key.rsplit_once('#') {
        Some((endpoint, _hash)) => endpoint.to_string(),
        None => key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RetryConfig;

    #[test]
    fn host_label_strips_key_hash() {
        assert_eq!(
            host_label("https://api.z.ai/v4#deadbeef"),
            "https://api.z.ai/v4"
        );
        // Keyless endpoints have no suffix and pass through unchanged.
        assert_eq!(host_label("https://api.z.ai/v4"), "https://api.z.ai/v4");
    }

    #[test]
    fn at_rest_no_endpoint_is_throttled() {
        let http = HttpClient::new();
        // Resolving an endpoint alone (no 429, no in-flight requests) is at rest.
        let _ = http.endpoint("https://api.rest/v1", None, None);
        assert!(http.throttle_status().is_none());
    }

    #[test]
    fn retry_after_window_surfaces_as_backoff() {
        let http = HttpClient::new();
        let ep = http.endpoint("https://api.parked/v1", None, None);
        ep.set_retry_after(Duration::from_secs(30));
        let status = http
            .throttle_status()
            .expect("parked endpoint is throttled");
        assert_eq!(status.endpoint, "https://api.parked/v1");
        let remaining = status.backoff_remaining.expect("cool-down window present");
        assert!(
            remaining > Duration::from_secs(25) && remaining <= Duration::from_secs(30),
            "got {remaining:?}"
        );
    }

    #[test]
    fn penalized_pacing_surfaces_without_a_cool_down() {
        let http = HttpClient::new();
        let ep = http.endpoint("https://api.slowed/v1", None, None);
        ep.limiter.penalize(); // slow the gate below base, no Retry-After set
        let status = http
            .throttle_status()
            .expect("penalized endpoint is throttled");
        assert!(status.penalized);
        assert!(status.backoff_remaining.is_none());
    }

    #[test]
    fn saturated_in_flight_cap_surfaces_as_throttled() {
        let http = HttpClient::with_config(RetryConfig {
            concurrency: 1,
            ..RetryConfig::default()
        });
        let ep = http.endpoint("https://api.busy/v1", None, None);
        // Hold the only permit → in_flight == cap, so the endpoint is throttled
        // even without a 429.
        let _permit = ep.concurrency.clone().try_acquire_owned().unwrap();
        let status = http
            .throttle_status()
            .expect("saturated endpoint is throttled");
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.cap, 1);
    }

    #[test]
    fn longest_cool_down_wins_across_endpoints() {
        let http = HttpClient::new();
        http.endpoint("https://api.a/v1", None, None)
            .set_retry_after(Duration::from_secs(5));
        http.endpoint("https://api.b/v1", None, None)
            .set_retry_after(Duration::from_secs(120));
        let status = http.throttle_status().expect("some endpoint is throttled");
        assert_eq!(status.endpoint, "https://api.b/v1");
    }

    #[test]
    fn saturated_model_slot_surfaces_as_the_binding_cap() {
        // The endpoint itself is nowhere near its (default 3) cap, but a
        // model-level slot (#521) at 1/1 is more constrained — it must be the
        // one `throttle_status` reports.
        let http = HttpClient::new();
        let ep = http.endpoint("https://api.zai/v1", None, None);
        let flash = ep.model_slot("glm-4.7-flash", 1);
        let _permit = flash.semaphore.clone().try_acquire_owned().unwrap();
        let status = http
            .throttle_status()
            .expect("a saturated model slot is throttled");
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.cap, 1);
        assert_eq!(status.model.as_deref(), Some("glm-4.7-flash"));
    }

    #[test]
    fn an_idle_model_slot_never_shadows_a_throttled_endpoint() {
        // A registered-but-empty model slot (0/5, e.g. GLM-5.2) must not hide
        // the endpoint's own genuine cool-down — the endpoint stays binding.
        let http = HttpClient::new();
        let ep = http.endpoint("https://api.zai/v1", None, None);
        let _glm52 = ep.model_slot("glm-5.2", 5);
        ep.set_retry_after(Duration::from_secs(10));
        let status = http.throttle_status().expect("endpoint is parked");
        assert_eq!(status.model, None);
        assert!(status.backoff_remaining.is_some());
    }

    #[test]
    fn a_model_with_no_cap_never_appears_as_binding() {
        // A request for a model with no per-model cap never creates a slot
        // (`execute_with_retry` only calls `model_slot` when `Some`), so
        // `throttle_status` reports the endpoint alone — byte-identical to
        // before #521.
        let http = HttpClient::new();
        let _ = http.endpoint("https://api.rest/v1", None, None);
        assert!(http.throttle_status().is_none());
    }
}
