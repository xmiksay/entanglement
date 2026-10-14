//! The retrying request driver ([`HttpClient::execute_with_retry`]); its
//! backoff / `Retry-After` / error-classification helpers live in `backoff.rs`.

use std::time::Instant;

use tokio::time::{sleep, timeout};

use super::backoff::{is_retryable_status, is_transient_error, next_backoff, parse_retry_after};
use super::endpoint::{pool_key, WaiterGuard};
use super::{HttpClient, RetryConfig, RetryError, StreamGuard};

impl HttpClient {
    /// Execute a request with per-endpoint rate-limiting and retry/backoff. The
    /// RPM budget + `Retry-After` window are keyed by `(endpoint, api_key)`: the
    /// provider's base URL **plus** the API key (if any), so multiple keys on the
    /// same endpoint each get their own budget — different keys have different
    /// limits (#217). `rpm`/`concurrency` are the endpoint's catalog-provided
    /// per-minute budget / in-flight cap (`None` → the pool defaults, #414).
    /// `model`/`model_concurrency` (#521) are the requested model's id and its
    /// optional tighter per-model in-flight cap on this same endpoint — `None`
    /// admits solely through the endpoint-wide cap, byte-identical to pre-#521.
    /// The endpoint cap is the ceiling on the *sum* of in-flight requests across
    /// every model sharing it; each model's own cap bounds only itself. When
    /// `self` carries a per-user budget (#632, ADR-0175, attached via
    /// `with_user_budget`), that user's own rpm/concurrency slot on this
    /// endpoint is admitted through too — narrower still, so two users
    /// sharing one literal key each stay within their own slice regardless of
    /// which one's session happened to size the endpoint first. Permits
    /// acquire **user, then model, then endpoint, then the cross-process
    /// shared lease last** (#546) — a caller blocked on its own user or model
    /// slot never holds a scarcer resource hostage, which would otherwise
    /// starve sibling users/models (in-process) or sibling `skutter`
    /// processes (cross-process) — and release in the reverse order.
    ///
    /// Returns the response **plus a [`StreamGuard`]** the caller must keep alive
    /// for the whole streamed body — it holds the per-endpoint concurrency permit
    /// (the storm guard for many spawned sub-agents) and, when set, the per-model
    /// permit too. A **429 parks the whole endpoint and retries** on a growing
    /// wait until it clears *or* `rate_limit_max_elapsed` is exceeded, then
    /// surfaces as `Ok` for the caller to error (so a saturated endpoint fails a
    /// turn rather than hanging its parent) — the cool-down stays endpoint-wide
    /// (v1; see ADR-0140), not scoped to just the offending model. The
    /// endpoint-wide park itself is clamped to `rate_limit_max_backoff`
    /// regardless of how large the server's `Retry-After` was (#547): every
    /// *other* caller of this endpoint — in-process and, once restarted,
    /// cross-process via the persisted shared file — waits at most that long,
    /// even though the offending caller's own give-up decision still honors
    /// the server's raw value. Every wait this call makes (the endpoint-wide
    /// cool-down and the cross-process gate) is itself bounded by this call's
    /// own `rate_limit_max_elapsed`, surfacing `RetryError::RateLimited`
    /// rather than sleeping past it. Transient transport faults and 5xx retry
    /// up to `max_attempts`; a permanent 4xx or an exhausted retryable is
    /// returned as `Ok`.
    ///
    /// `retry` overrides the pool's own [`RetryConfig`] for this call only —
    /// `None` uses the pool default, exactly like before this parameter
    /// existed. The RPM/concurrency/shared-lease admission gates above are
    /// unaffected either way (they key off `endpoint`, not `config`): only
    /// the failure-path knobs (`max_attempts`, backoff, `response_header_
    /// timeout`) change. Exists for a caller with a legitimately different
    /// patience budget than ordinary LLM traffic — e.g. a display-only aux
    /// request (narrate/session-title) that should fail fast rather than eat the LLM-tuned retry
    /// ladder (#660, #560) — while still riding the same endpoint's RPM/
    /// concurrency/429 admission as everyone else.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_with_retry<F, Fut>(
        &self,
        endpoint: &str,
        api_key: Option<&str>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        model: &str,
        model_concurrency: Option<usize>,
        retry: Option<RetryConfig>,
        request_fn: F,
    ) -> Result<(reqwest::Response, StreamGuard), RetryError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let endpoint = self.endpoint(&pool_key(endpoint, api_key), rpm, concurrency);
        let model_slot = model_concurrency.map(|cap| endpoint.model_slot(model, cap));
        let user_slot = self.user_budget.as_ref().map(|b| endpoint.user_slot(b));
        let config = retry.unwrap_or(self.pool.config);
        // `attempt` bounds only *genuine failures* (5xx / transport faults). A
        // 429 is "wait your turn": it retries until it clears (not counted here),
        // bounded overall by `rate_limit_max_elapsed`.
        let mut attempt = 0;
        let mut backoff = config.initial_backoff;
        let mut rl_backoff = config.rate_limit_initial_backoff;
        let rl_deadline = Instant::now() + config.rate_limit_max_elapsed;

        loop {
            if endpoint.wait_for_retry_after(rl_deadline).await.is_err() {
                tracing::error!(
                    "rate limited: giving up after exhausting the retry budget waiting \
                     for the endpoint's retry-after cool-down to clear"
                );
                return Err(RetryError::RateLimited);
            }
            endpoint.limiter.acquire().await;
            // User permit **first** (#632, ADR-0175): the narrowest,
            // most caller-specific gate goes first so a caller blocked on
            // its own user slot never holds a resource shared with other
            // users/models hostage while it waits.
            let user_permit = match &user_slot {
                Some(slot) => slot.acquire().await,
                None => None,
            };
            // Model permit next (#521 refinement): a caller blocked on its
            // model's own slot must never hold a scarcer resource hostage
            // while it waits — that would let one saturated model starve every
            // *other* model sharing the endpoint (or every sibling process
            // sharing the endpoint's cross-process lease, #546) of admission,
            // even though the endpoint itself has room. Acquiring in this
            // fixed order (user, model, then endpoint, then the shared lease)
            // everywhere is also deadlock-free. Released in the reverse order
            // below/in `StreamGuard`'s field order.
            let model_permit = match &model_slot {
                Some(slot) => Some(
                    slot.semaphore
                        .clone()
                        .acquire_owned()
                        .await
                        .expect("model concurrency semaphore never closed"),
                ),
                None => None,
            };
            // Bound in-flight requests to this endpoint — the ceiling on the
            // *sum* across every model sharing it. The permit is held until the
            // returned `StreamGuard` drops (i.e. the stream is consumed);
            // dropped here on any retry so a queued caller can take the slot.
            // `_waiter` counts this caller as queued for display only
            // (#517, #552) — released the instant the
            // permit is granted, via `WaiterGuard`'s `Drop` so a cancelled
            // await (Stop) still decrements it.
            let _waiter = WaiterGuard::new(&endpoint.waiters);
            let permit = endpoint
                .concurrency
                .clone()
                .acquire_owned()
                .await
                .expect("endpoint concurrency semaphore never closed");
            drop(_waiter);
            // Cross-process gate (#523, ADR-0144), acquired **last** (#546):
            // admitted only once every `skutter` process sharing this
            // endpoint+key agrees there's combined RPM/concurrency room and no
            // peer's `Retry-After` is still in force. Acquiring it only after
            // both in-process permits are already held means a caller queued
            // on its model's semaphore never sits holding this scarcer,
            // cross-process-wide resource — and that the shared RPM ledger,
            // stamped on admission below, is stamped right before the request
            // actually fires. `None` when sharing is disabled or the shared
            // state directory is unwritable — falls back to the in-process
            // gates above unaffected.
            let shared_lease = match endpoint
                .shared
                .acquire(endpoint.rpm, endpoint.concurrency_cap, rl_deadline)
                .await
            {
                Ok(lease) => lease,
                Err(()) => {
                    // The cross-process gate would need to wait past this
                    // caller's own `rate_limit_max_elapsed` budget (#547) —
                    // `permit`/`model_permit` drop here, freeing the
                    // in-process slots this caller was holding hostage.
                    tracing::error!(
                        "rate limited: giving up after exhausting the retry budget waiting \
                         for the cross-process shared endpoint gate"
                    );
                    return Err(RetryError::RateLimited);
                }
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
                    drop(permit);
                    drop(model_permit);
                    drop(user_permit);
                    drop(shared_lease);
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
                    // Success: recover the endpoint's pacing a notch, hand back
                    // the response together with the held concurrency permits.
                    if status.is_success() {
                        endpoint.limiter.relax();
                        return Ok((
                            response,
                            StreamGuard(permit, model_permit, user_permit, shared_lease),
                        ));
                    }
                    // Permanent 4xx: hand it straight back — the caller inspects
                    // `!is_success()`. (Permits drop as the guard is dropped by
                    // the caller after reading the body.)
                    if !is_retryable_status(status) {
                        return Ok((
                            response,
                            StreamGuard(permit, model_permit, user_permit, shared_lease),
                        ));
                    }
                    let retry_after = parse_retry_after(response.headers());

                    // 429 "too many requests": release the in-flight slots, park
                    // the *whole* endpoint (every concurrent caller backs off
                    // together), slow the pacing gate, and retry on a growing wait
                    // — until it clears or the overall `rate_limit_max_elapsed`
                    // budget is spent, at which point surface it as an error.
                    if status.as_u16() == 429 {
                        endpoint.limiter.penalize();
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
                        endpoint.set_retry_after(endpoint_delay);
                        endpoint.shared.mark_retry_after(endpoint_delay).await;
                        // Give up once another wait would exceed the overall
                        // budget: surface the 429 (permits still held) so the
                        // caller errors instead of the parent hanging forever.
                        if Instant::now() + delay >= rl_deadline {
                            tracing::error!(
                                status = %status,
                                "rate limited (429): giving up after exhausting the retry budget"
                            );
                            return Ok((
                                response,
                                StreamGuard(permit, model_permit, user_permit, shared_lease),
                            ));
                        }
                        drop(permit); // free the slots while we back off
                        drop(model_permit);
                        drop(user_permit);
                        drop(shared_lease);
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
                        endpoint.set_retry_after(server_delay.min(config.rate_limit_max_backoff));
                    }
                    if attempt >= config.max_attempts {
                        return Ok((
                            response,
                            StreamGuard(permit, model_permit, user_permit, shared_lease),
                        ));
                    }
                    drop(permit);
                    drop(model_permit);
                    drop(user_permit);
                    drop(shared_lease);
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
                    drop(permit);
                    drop(model_permit);
                    drop(user_permit);
                    drop(shared_lease);
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
