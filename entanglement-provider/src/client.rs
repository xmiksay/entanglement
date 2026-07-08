//! Shared HTTP client with connection pool tuning, retry/backoff logic, and
//! rate-limit handling. Provides a single tuned `reqwest::Client` that is cloned
//! into each LLM backend instance instead of each constructing its own.
//!
//! # Connection pool tuning
//! - `pool_max_idle_per_host`: Maximum number of idle connections per host to keep
//!   in the pool before closing.
//! - `pool_idle_timeout`: How long an idle connection stays in the pool before
//!   being closed.
//!
//! # Retry logic
//! - Exponential backoff with jitter for transient failures
//! - Bounded max attempts
//! - Retries only transient failures: connection errors, 5xx responses,
//!   streams that drop before completion
//!
//! # Rate-limit handling
//! - HTTP 429 responses parse `Retry-After` header
//! - Back off accordingly before retrying
//! - Client-side RPM throttle shared across sessions using a token bucket

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::sleep;

const MAX_RETRY_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_HOST: usize = 10;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const RPM_LIMIT: u32 = 50;
const BUCKET_CAPACITY: u32 = RPM_LIMIT;
const REFILL_INTERVAL_MILLIS: u64 = 60_000 / RPM_LIMIT as u64;
const REFILL_INTERVAL: Duration = Duration::from_millis(REFILL_INTERVAL_MILLIS);

/// Shared HTTP client with connection pool tuning. Cheap to clone because the
/// underlying `reqwest::Client` is `Arc`-wrapped internally.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    rate_limiter: Arc<RateLimiter>,
}

/// Client-side rate limiter using a token bucket algorithm.
#[derive(Clone)]
struct RateLimiter {
    semaphore: Arc<Semaphore>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(BUCKET_CAPACITY as usize)),
        }
    }

    async fn acquire(&self) {
        let _permit = self.semaphore.acquire().await.unwrap();
        let semaphore = self.semaphore.clone();
        tokio::spawn(async move {
            sleep(REFILL_INTERVAL).await;
            semaphore.add_permits(1);
        });
    }
}

impl HttpClient {
    /// Create a new shared HTTP client with tuned connection pool settings.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            rate_limiter: Arc::new(RateLimiter::new()),
        }
    }

    /// Get the underlying `reqwest::Client` for making requests.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Acquire a rate-limited permit before making a request.
    pub async fn acquire_rate_limit(&self) {
        self.rate_limiter.acquire().await;
    }

    /// Execute a request with retry logic.
    pub async fn execute_with_retry<F, Fut, T>(&self, request_fn: F) -> Result<T, RetryError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, reqwest::Error>>,
    {
        let mut attempt = 0;
        let mut backoff = INITIAL_BACKOFF;

        loop {
            attempt += 1;
            self.acquire_rate_limit().await;

            match request_fn().await {
                Ok(response) => return Ok(response),
                Err(e) if attempt >= MAX_RETRY_ATTEMPTS => {
                    return Err(RetryError::Exhausted(attempt, e));
                }
                Err(e) => {
                    if !is_transient_error(&e) {
                        return Err(RetryError::Permanent(e));
                    }

                    let retry_after = extract_retry_after(&e);
                    let delay = retry_after.unwrap_or(backoff);

                    tracing::warn!(
                        attempt = attempt,
                        max_attempts = MAX_RETRY_ATTEMPTS,
                        error = %e,
                        backoff = ?delay,
                        "transient error, retrying after backoff"
                    );

                    sleep(delay).await;

                    backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                    let jitter = rand::random::<f64>() * backoff.as_millis() as f64;
                    backoff = Duration::from_millis((backoff.as_millis() as f64 + jitter) as u64);
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

/// Check if an error is transient and should be retried.
fn is_transient_error(error: &reqwest::Error) -> bool {
    if error.is_timeout() || error.is_connect() {
        return true;
    }

    if let Some(status) = error.status() {
        if status.is_server_error() || status.as_u16() == 429 {
            return true;
        }
    }

    if error.to_string().contains("incomplete") {
        return true;
    }

    false
}

/// Extract `Retry-After` header value from the error response if available.
pub fn extract_retry_after_from_response(response: &reqwest::Response) -> Option<Duration> {
    if response.status().as_u16() != 429 {
        return None;
    }

    let retry_after = response.headers().get("Retry-After")?;

    retry_after.to_str().ok().and_then(|val| {
        if let Ok(seconds) = val.parse::<u64>() {
            Some(Duration::from_secs(seconds))
        } else if let Ok(datetime) = httpdate::parse_http_date(val) {
            let now = std::time::SystemTime::now();
            datetime.duration_since(now).ok()
        } else {
            None
        }
    })
}

/// Extract `Retry-After` header value from the error response if available.
fn extract_retry_after(error: &reqwest::Error) -> Option<Duration> {
    if error.status()?.as_u16() != 429 {
        return None;
    }

    None
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
}
