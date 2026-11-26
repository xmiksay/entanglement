//! The retrying request driver ([`HttpClient::execute_with_retry`]); its
//! backoff / `Retry-After` / error-classification helpers live in `backoff.rs`.

use std::time::Instant;

use tokio::time::{sleep, timeout};

use super::backoff::{is_retryable_status, is_transient_error, next_backoff, parse_retry_after};
use super::endpoint::pool_key;
use super::{HttpClient, RequestLimits, RetryConfig, RetryError, StreamGuard};

impl HttpClient {
    /// Send one request through its endpoint's limiter, retrying per
    /// [`RetryConfig`]. `endpoint` + `api_key` pick the limiter (different
    /// keys on one URL get independent budgets); `limits` are the endpoint's
    /// and `model`'s [`RequestLimits`]; `tokens` is the request's estimated
    /// input size, counted against a `tpm` window (0 when unknown).
    ///
    /// Returns the response **plus a [`StreamGuard`]** the caller keeps alive
    /// for the whole streamed body. A **429 parks the whole endpoint** for the
    /// server's `Retry-After` (else a growing wait) and retries until it clears
    /// or `rate_limit_max_elapsed` is spent, then comes back as `Ok` for the
    /// caller to error on. The endpoint-wide park is clamped to
    /// `rate_limit_max_backoff` so a huge `Retry-After` never parks every other
    /// caller (or, via the shared file, a restarted process) that long; this
    /// caller's own give-up decision still honors the raw value. Every wait is
    /// bounded by this call's `rate_limit_max_elapsed`
    /// (`RetryError::RateLimited`). Transport faults and 5xx retry up to
    /// `max_attempts`; a permanent 4xx or an exhausted retryable is `Ok`.
    ///
    /// `retry` overrides the pool's [`RetryConfig`] for this call only (a
    /// display-only aux request that must fail fast); admission is unaffected.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_with_retry<F, Fut>(
        &self,
        endpoint: &str,
        api_key: Option<&str>,
        model: &str,
        limits: RequestLimits,
        tokens: u32,
        retry: Option<RetryConfig>,
        request_fn: F,
    ) -> Result<(reqwest::Response, StreamGuard), RetryError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let limiter = self.endpoint(&pool_key(endpoint, api_key), limits.endpoint);
        let config = retry.unwrap_or(self.pool.config);
        // `attempt` bounds only *genuine failures* (5xx / transport faults). A
        // 429 is "wait your turn": it retries until it clears (not counted here),
        // bounded overall by `rate_limit_max_elapsed`.
        let mut attempt = 0;
        let mut backoff = config.initial_backoff;
        let mut rl_backoff = config.rate_limit_initial_backoff;
        let rl_deadline = Instant::now() + config.rate_limit_max_elapsed;

        loop {
            if limiter.wait_for_retry_after(rl_deadline).await.is_err() {
                tracing::error!(
                    "rate limited: giving up after exhausting the retry budget waiting \
                     for the endpoint's retry-after cool-down to clear"
                );
                return Err(RetryError::RateLimited);
            }
            let Ok(guard) = limiter.admit(model, limits, tokens, rl_deadline).await else {
                tracing::error!(
                    "rate limited: giving up after exhausting the retry budget waiting \
                     for the cross-process shared endpoint gate"
                );
                return Err(RetryError::RateLimited);
            };

            // Bounded by `response_header_timeout` (#658): a server that accepts
            // the connection and takes the request but never writes headers
            // would otherwise hang here forever, past both `CONNECT_TIMEOUT`
            // (establishment only) and `STREAM_IDLE_TIMEOUT` (which needs a
            // `Response` to have started). An elapsed timeout drops the
            // in-flight future — aborting the silent connection — and is
            // classified exactly like a transient transport fault so the
            // retry loop below re-issues it.
            match timeout(config.response_header_timeout, request_fn()).await {
                Err(_elapsed) => {
                    drop(guard);
                    attempt += 1;
                    if attempt >= config.max_attempts {
                        return Err(RetryError::HeaderTimeout(
                            config.response_header_timeout,
                            attempt,
                        ));
                    }
                    tracing::warn!(
                        attempt,
                        max_attempts = config.max_attempts,
                        timeout = ?config.response_header_timeout,
                        backoff = ?backoff,
                        "no response headers within timeout, retrying after backoff"
                    );
                    sleep(backoff).await;
                    backoff = next_backoff(backoff, config.max_backoff);
                }
                Ok(Ok(response)) => {
                    let status = response.status();
                    // Success: hand back the response with the held permits.
                    if status.is_success() {
                        return Ok((response, guard));
                    }
                    // Permanent 4xx: hand it straight back — the caller inspects
                    // `!is_success()`. (Permits drop as the guard is dropped by
                    // the caller after reading the body.)
                    if !is_retryable_status(status) {
                        return Ok((response, guard));
                    }
                    let retry_after = parse_retry_after(response.headers());

                    // 429 "too many requests": release the in-flight slots, park
                    // the *whole* endpoint (every concurrent caller backs off
                    // together), and retry on a growing wait
                    // — until it clears or the overall `rate_limit_max_elapsed`
                    // budget is spent, at which point surface it as an error.
                    if status.as_u16() == 429 {
                        let delay = retry_after
                            .unwrap_or_else(|| rl_backoff.min(config.rate_limit_max_backoff));
                        // The endpoint-wide park (every other caller waits on
                        // this, in-process via `wait_for_retry_after` and
                        // cross-process via the persisted shared file) is
                        // clamped to `rate_limit_max_backoff` regardless of
                        // how large the server's own `Retry-After` was
                        // (#547) — a `Retry-After: 3600` must not park every
                        // *other* caller of this endpoint for an hour, nor
                        // survive a restart parked that long via the shared
                        // file. `delay` itself stays uncapped (up to
                        // `MAX_RETRY_AFTER`, #548) for *this* caller's own
                        // give-up decision below, honoring what the server
                        // actually asked for.
                        let endpoint_delay = delay.min(config.rate_limit_max_backoff);
                        limiter.set_retry_after(endpoint_delay);
                        limiter.shared.mark_retry_after(endpoint_delay).await;
                        // Give up once another wait would exceed the overall
                        // budget: surface the 429 (permits still held) so the
                        // caller errors instead of the parent hanging forever.
                        if Instant::now() + delay >= rl_deadline {
                            tracing::error!(
                                status = %status,
                                "rate limited (429): giving up after exhausting the retry budget"
                            );
                            return Ok((response, guard));
                        }
                        drop(guard); // free the slots while we back off
                        tracing::warn!(
                            status = %status,
                            backoff = ?delay,
                            "rate limited (429): parking endpoint, retrying until clear"
                        );
                        sleep(delay).await;
                        rl_backoff = next_backoff(rl_backoff, config.rate_limit_max_backoff);
                        continue;
                    }

                    // Retryable 5xx: bounded by `max_attempts`; park only if the
                    // server advised a `Retry-After`. Clamped the same way as
                    // the 429 path above (#547) — the endpoint-wide park
                    // every caller waits on must never exceed
                    // `rate_limit_max_backoff`, even though this caller's own
                    // `sleep(delay)` below still honors the raw value.
                    attempt += 1;
                    if let Some(server_delay) = retry_after {
                        limiter.set_retry_after(server_delay.min(config.rate_limit_max_backoff));
                    }
                    if attempt >= config.max_attempts {
                        return Ok((response, guard));
                    }
                    drop(guard);
                    let delay = retry_after.unwrap_or(backoff);
                    tracing::warn!(
                        attempt,
                        max_attempts = config.max_attempts,
                        status = %status,
                        backoff = ?delay,
                        "retryable server error, retrying after backoff"
                    );
                    sleep(delay).await;
                    backoff = next_backoff(backoff, config.max_backoff);
                }
                Ok(Err(e)) if !is_transient_error(&e) => return Err(RetryError::Permanent(e)),
                Ok(Err(e)) => {
                    drop(guard);
                    attempt += 1;
                    if attempt >= config.max_attempts {
                        return Err(RetryError::Exhausted(attempt, e));
                    }
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
