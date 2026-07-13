//! Shared HTTP transport with a **per-endpoint** connection pool, rate-limit
//! budget, and retry/backoff. One tuned `reqwest::Client` (which already pools
//! connections per host) is shared across every backend; the resilience state —
//! RPM throttle and `Retry-After` window — is keyed by endpoint so talking to
//! different APIs is isolated: one throttled endpoint never starves another
//! (#217).
//!
//! # Connection pool tuning
//! - `pool_max_idle_per_host`: idle connections kept per host before closing.
//! - `pool_idle_timeout`: how long an idle connection lingers before closing.
//!
//! # Retry logic (per endpoint)
//! Retries transient failures — connect/timeout faults, dropped streams, and
//! **retryable HTTP responses (429 / 5xx)** classified *inside* the loop — with
//! exponential backoff + jitter, bounded by `max_attempts`. Before #217 a 429/5xx
//! *response* came back as `reqwest::Ok` and so was never retried (#193): the
//! classification now happens on the `Response`, not just on `reqwest::Error`.
//!
//! # Rate-limit handling (per endpoint)
//! Each endpoint owns a token-bucket RPM throttle and a `Retry-After` window: a
//! 429/5xx with `Retry-After` parks every caller of *that* endpoint until the
//! window elapses, leaving other endpoints untouched.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::time::sleep;

const MAX_RETRY_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_HOST: usize = 10;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const RPM_LIMIT: u32 = 50;

/// Retry/rate-limit tuning applied per endpoint. Defaults match the historical
/// shared client (5 attempts, 200ms→30s backoff, 50 RPM) — now *per endpoint*
/// rather than global.
#[derive(Clone, Copy, Debug)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Requests per minute allowed to each endpoint before throttling.
    pub rpm: u32,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: MAX_RETRY_ATTEMPTS,
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
            rpm: RPM_LIMIT,
        }
    }
}

impl RetryConfig {
    /// A config that never retries (one attempt). Handy for tests that assert the
    /// surfaced error of a single failing response without incurring backoff.
    pub fn no_retry() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }
}

/// Shared HTTP client + per-endpoint resilience pool. Cheap to clone: the
/// `reqwest::Client` is `Arc`-wrapped internally and the endpoint pool is shared
/// behind an `Arc`.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    pool: Arc<EndpointPool>,
}

/// Per-endpoint resilience state, lazily created on first use and keyed by the
/// endpoint's base URL. Each endpoint carries its own RPM budget + `Retry-After`
/// window so throttling on one API never blocks another.
struct EndpointPool {
    endpoints: Mutex<HashMap<String, Arc<EndpointState>>>,
    config: RetryConfig,
}

/// One endpoint's live rate-limit + backoff state.
struct EndpointState {
    limiter: RateLimiter,
    /// Instant before which no request to this endpoint may proceed, set from a
    /// `Retry-After` header. `None` = no active cool-down.
    retry_after: Mutex<Option<Instant>>,
}

impl EndpointState {
    fn new(rpm: u32) -> Self {
        Self {
            limiter: RateLimiter::new(rpm),
            retry_after: Mutex::new(None),
        }
    }

    /// Extend (never shorten) this endpoint's cool-down window by `delay`.
    fn set_retry_after(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut guard = self.retry_after.lock().expect("retry_after poisoned");
        if guard.is_none_or(|cur| until > cur) {
            *guard = Some(until);
        }
    }

    /// Park until any active `Retry-After` window has elapsed.
    async fn wait_for_retry_after(&self) {
        let until = *self.retry_after.lock().expect("retry_after poisoned");
        if let Some(until) = until {
            let now = Instant::now();
            if until > now {
                sleep(until - now).await;
            }
        }
    }
}

/// Client-side rate limiter using a token-bucket: capacity `rpm` tokens, one
/// refilled every `60s / rpm`. Each `acquire` **consumes** a token (the permit is
/// forgotten, not released on drop) and schedules its return — the pre-#217 code
/// released the permit immediately, so it never actually throttled (#193).
struct RateLimiter {
    semaphore: Arc<Semaphore>,
    refill_interval: Duration,
}

impl RateLimiter {
    fn new(rpm: u32) -> Self {
        let rpm = rpm.max(1);
        Self {
            semaphore: Arc::new(Semaphore::new(rpm as usize)),
            refill_interval: Duration::from_millis(60_000 / rpm as u64),
        }
    }

    async fn acquire(&self) {
        // Take a token and keep it (forget the permit) so capacity actually
        // drops; a spawned timer returns it after the refill interval.
        match self.semaphore.acquire().await {
            Ok(permit) => permit.forget(),
            Err(_) => return, // semaphore is never closed (we hold an Arc to it)
        }
        let semaphore = self.semaphore.clone();
        let interval = self.refill_interval;
        tokio::spawn(async move {
            sleep(interval).await;
            semaphore.add_permits(1);
        });
    }
}

impl HttpClient {
    /// Create a shared HTTP client with default per-endpoint retry/rate-limit.
    pub fn new() -> Self {
        Self::with_config(RetryConfig::default())
    }

    /// Create a shared HTTP client with a custom per-endpoint [`RetryConfig`].
    pub fn with_config(config: RetryConfig) -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            pool: Arc::new(EndpointPool {
                endpoints: Mutex::new(HashMap::new()),
                config,
            }),
        }
    }

    /// Get the underlying `reqwest::Client` for making requests.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Resolve (creating on first use) the resilience state for `endpoint`.
    fn endpoint(&self, endpoint: &str) -> Arc<EndpointState> {
        let mut map = self.pool.endpoints.lock().expect("endpoint pool poisoned");
        map.entry(endpoint.to_string())
            .or_insert_with(|| Arc::new(EndpointState::new(self.pool.config.rpm)))
            .clone()
    }

    /// Execute a request against `endpoint` with per-endpoint rate-limiting and
    /// retry/backoff. `endpoint` keys the RPM budget + `Retry-After` window (use
    /// the provider's base URL). Retries transient transport faults and retryable
    /// HTTP responses (429 / 5xx); a permanent 4xx or an exhausted retryable
    /// response is returned as `Ok` for the caller to surface.
    pub async fn execute_with_retry<F, Fut>(
        &self,
        endpoint: &str,
        request_fn: F,
    ) -> Result<reqwest::Response, RetryError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let endpoint = self.endpoint(endpoint);
        let config = self.pool.config;
        let mut attempt = 0;
        let mut backoff = config.initial_backoff;

        loop {
            attempt += 1;
            endpoint.wait_for_retry_after().await;
            endpoint.limiter.acquire().await;

            match request_fn().await {
                Ok(response) => {
                    let status = response.status();
                    // Success or a permanent 4xx: hand it straight back — the
                    // caller inspects `!is_success()` for the permanent case.
                    if status.is_success() || !is_retryable_status(status) {
                        return Ok(response);
                    }
                    // Retryable 429/5xx but out of attempts: surface the response.
                    if attempt >= config.max_attempts {
                        return Ok(response);
                    }
                    // Retryable: honor `Retry-After` (parking the whole endpoint)
                    // else exponential backoff, then retry.
                    let retry_after = parse_retry_after(response.headers());
                    if let Some(delay) = retry_after {
                        endpoint.set_retry_after(delay);
                    }
                    let delay = retry_after.unwrap_or(backoff);
                    tracing::warn!(
                        attempt,
                        max_attempts = config.max_attempts,
                        status = %status,
                        backoff = ?delay,
                        "retryable HTTP status, retrying after backoff"
                    );
                    sleep(delay).await;
                    backoff = next_backoff(backoff, config.max_backoff);
                }
                Err(e) if !is_transient_error(&e) => return Err(RetryError::Permanent(e)),
                Err(e) if attempt >= config.max_attempts => {
                    return Err(RetryError::Exhausted(attempt, e));
                }
                Err(e) => {
                    tracing::warn!(
                        attempt,
                        max_attempts = config.max_attempts,
                        error = %e,
                        backoff = ?backoff,
                        "transient error, retrying after backoff"
                    );
                    sleep(backoff).await;
                    backoff = next_backoff(backoff, config.max_backoff);
                }
            }
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Grow the backoff geometrically, cap it, then add up to 100% jitter.
fn next_backoff(backoff: Duration, max: Duration) -> Duration {
    let capped = std::cmp::min(backoff * 2, max);
    let jitter = rand::random::<f64>() * capped.as_millis() as f64;
    Duration::from_millis((capped.as_millis() as f64 + jitter) as u64)
}

/// A retryable HTTP status: server errors and 429 Too Many Requests.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status.as_u16() == 429
}

/// Check if a transport error is transient and should be retried.
fn is_transient_error(error: &reqwest::Error) -> bool {
    if error.is_timeout() || error.is_connect() {
        return true;
    }
    if let Some(status) = error.status() {
        if is_retryable_status(status) {
            return true;
        }
    }
    error.to_string().contains("incomplete")
}

/// Parse a `Retry-After` header (delta-seconds or an HTTP date) into a duration.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let val = headers.get("Retry-After")?.to_str().ok()?;
    if let Ok(seconds) = val.parse::<u64>() {
        Some(Duration::from_secs(seconds))
    } else if let Ok(datetime) = httpdate::parse_http_date(val) {
        datetime.duration_since(std::time::SystemTime::now()).ok()
    } else {
        None
    }
}

/// Extract a `Retry-After` duration from a 429 response, for callers that want to
/// report the server-advised backoff when surfacing a rate-limit error.
pub fn extract_retry_after_from_response(response: &reqwest::Response) -> Option<Duration> {
    if response.status().as_u16() != 429 {
        return None;
    }
    parse_retry_after(response.headers())
}

/// Retry error types.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("max retry attempts ({0}) exhausted: {1}")]
    Exhausted(u32, reqwest::Error),
    #[error("permanent error, not retrying: {0}")]
    Permanent(reqwest::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_http_client() {
        let _client = HttpClient::default();
    }

    #[test]
    fn test_new_http_client() {
        let _client = HttpClient::new();
    }

    #[test]
    fn endpoints_are_isolated_and_stable_by_key() {
        let http = HttpClient::new();
        let a1 = http.endpoint("https://api.a/v1");
        let a2 = http.endpoint("https://api.a/v1");
        let b = http.endpoint("https://api.b/v1");
        // Same key → same state; different keys → isolated state.
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!Arc::ptr_eq(&a1, &b));
    }

    #[test]
    fn retry_after_window_extends_never_shrinks() {
        let state = EndpointState::new(RPM_LIMIT);
        state.set_retry_after(Duration::from_secs(10));
        let long = state.retry_after.lock().unwrap().unwrap();
        // A shorter window must not overwrite a longer one.
        state.set_retry_after(Duration::from_secs(1));
        assert_eq!(state.retry_after.lock().unwrap().unwrap(), long);
        // A longer window does extend it.
        state.set_retry_after(Duration::from_secs(60));
        assert!(state.retry_after.lock().unwrap().unwrap() > long);
    }

    #[test]
    fn retryable_status_classification() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::OK));
    }

    #[test]
    fn parse_retry_after_reads_delta_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Retry-After", "12".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));
    }

    #[test]
    fn next_backoff_caps_at_max() {
        // Even from a large starting point the doubled+jittered value stays
        // within [max, 2*max).
        let max = Duration::from_secs(30);
        let d = next_backoff(Duration::from_secs(100), max);
        assert!(d >= max && d < max * 2, "got {d:?}");
    }
}
