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
    /// Requests currently in flight to this endpoint (held concurrency permits).
    pub in_flight: usize,
    /// The configured in-flight cap.
    pub cap: usize,
    /// Remaining time on an active `Retry-After` / 429 cool-down window, or
    /// `None` when the endpoint is not parked.
    pub backoff_remaining: Option<Duration>,
    /// Whether the adaptive pacing gate has slowed below its base rate (a 429
    /// penalized the interval), independent of an explicit cool-down window.
    pub penalized: bool,
}

impl EndpointState {
    /// Snapshot this endpoint's throttle posture. `endpoint` is the human label
    /// (base URL); the caller strips the pool-key hash suffix.
    fn status(&self, endpoint: String) -> ThrottleStatus {
        let in_flight = self
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
        ThrottleStatus {
            endpoint,
            in_flight,
            cap: self.concurrency_cap,
            backoff_remaining,
            penalized,
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
}
