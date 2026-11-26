//! Shared HTTP transport: one tuned `reqwest::Client` (pooling connections
//! per host) plus a per-endpoint [`limiter`] and the retry loop.
//!
//! # Limits (#918)
//! Every request admits through its endpoint's one limiter — keyed by
//! normalized base URL + API-key hash, so one throttled API (or key) never
//! blocks another. The limiter enforces [`RateLimits`] (`rpm`, `tpm`,
//! `concurrency`) on the endpoint and, when the requested model carries its
//! own `limits:`, on that model too (z.ai caps concurrency per model). The
//! limits come from the catalog entry per request ([`RequestLimits`]); unset
//! endpoint axes fall back to the client's defaults ([`RateLimits::DEFAULT`]:
//! 50 rpm, 3 in flight).
//! Permits are held for the whole request **and its streamed body**
//! ([`StreamGuard`]), so concurrency counts open streams. Sibling processes
//! sharing an endpoint + key also coordinate through a file-backed lease
//! (`shared_state`, ADR-0144).
//!
//! # Retries
//! Transport faults (connect/timeout/request-send, dropped streams) and
//! retryable 5xx retry with exponential backoff + jitter, bounded by
//! `max_attempts`. A **429 is "wait your turn"**: it parks the whole endpoint
//! for the server's `Retry-After` (or a patient self-computed wait, ≈5 s
//! ramping to ≈10 min) and retries until it clears or
//! `rate_limit_max_elapsed` is spent — never consuming the failure budget.
//! The cool-down lives in the limiter, so every caller of the endpoint waits
//! it out together.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::sync::OwnedSemaphorePermit;

mod backoff;
mod endpoint;
mod limiter;
mod retry;
mod shared_state;
mod shared_store;
mod status;
mod stream;
mod tls;
pub use backoff::extract_retry_after_from_response;
pub use limiter::{RateLimits, RequestLimits};
pub use shared_state::{prune_stale, SharedStateLocation};
pub use status::ThrottleStatus;
pub use stream::{log_request_body, spawn_byte_stream};
pub use tls::{client_builder, install_default_crypto_provider};

use endpoint::EndpointPool;

const MAX_RETRY_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_HOST: usize = 10;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Total wall-clock a single 429 will keep retrying before it gives up and
/// surfaces the response as an error. A concurrency-capped request should rarely
/// hit this; it exists so a genuinely saturated endpoint fails a sub-agent's
/// turn (returning a failed `ToolResult`) instead of hanging its parent forever.
const RATE_LIMIT_MAX_ELAPSED: Duration = Duration::from_secs(900);

/// First wait after a 429 with no server `Retry-After`. Rate-limit backoff has
/// its own schedule (separate from [`INITIAL_BACKOFF`]): a 429 is retried until
/// it clears, so it starts patient rather than hammering.
const RATE_LIMIT_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

/// Ceiling on the self-computed 429 wait. The schedule ramps geometrically from
/// [`RATE_LIMIT_INITIAL_BACKOFF`] up to here (≈10 min), then holds — so a
/// persistently rate-limited endpoint keeps polling at this cadence instead of
/// giving up. A server-provided `Retry-After` overrides this cap (honored as-is).
const RATE_LIMIT_MAX_BACKOFF: Duration = Duration::from_secs(600);

/// Ceiling a parsed `Retry-After` header is clamped to (#548). Generous enough
/// to honor any legitimate server-advised wait as-is, but bounded so a
/// network-controlled huge delta-seconds value or far-future HTTP-date can
/// never overflow the `Instant + Duration` arithmetic downstream.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Cap on establishing the TCP+TLS connection only — *not* the whole request.
/// A long healthy LLM stream must be allowed to run past any fixed ceiling, so
/// the old whole-request `.timeout(300s)` (which killed streams mid-turn and
/// blocked cancel for up to 5 min, #179/#241) is gone; liveness on the body is
/// enforced per-chunk by [`STREAM_IDLE_TIMEOUT`] instead.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle-gap timeout for a streaming response body: abort only when **no bytes**
/// arrive for this long. A slow-but-alive stream (long generation, reasoning)
/// runs to completion; a hung one still dies fast (#241).
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Default bound on the await-response-headers phase: from the moment the
/// request is handed to `reqwest` until a `Response` (headers) comes back.
/// [`CONNECT_TIMEOUT`] only bounds TCP+TLS establishment and
/// [`STREAM_IDLE_TIMEOUT`] only starts once a `Response` exists — a server
/// that accepts the connection, takes the request, then goes silent before
/// writing headers falls between both guards and hangs forever, pinning the
/// endpoint's concurrency permit and cross-process lease (#658). Aligned with
/// `STREAM_IDLE_TIMEOUT` since both bound the same kind of liveness gap, just
/// on either side of the headers. Overridable per pool via
/// [`RetryConfig::response_header_timeout`].
const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(120);

/// Retry tuning. `max_attempts`/`initial_backoff`/`max_backoff` bound the
/// *failure* path (5xx, transport faults); `rate_limit_*` is the separate 429
/// schedule. Admission limits are not here — they are [`RateLimits`].
#[derive(Clone, Copy, Debug)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// First wait after a 429 with no server `Retry-After` (a 429 retries until
    /// it clears, so it starts patient).
    pub rate_limit_initial_backoff: Duration,
    /// Ceiling on the self-computed 429 wait; the schedule ramps to here and
    /// holds. A server `Retry-After` overrides it (honored as-is).
    pub rate_limit_max_backoff: Duration,
    /// Total wall-clock a 429 retries before surfacing as an error (so a
    /// saturated endpoint fails a turn instead of hanging its parent forever).
    pub rate_limit_max_elapsed: Duration,
    /// Bound on the await-response-headers phase (#658) — see
    /// [`RESPONSE_HEADER_TIMEOUT`] for why this guard exists. A field (not a
    /// bare const) so tests can shrink it instead of waiting out the real
    /// value.
    pub response_header_timeout: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: MAX_RETRY_ATTEMPTS,
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
            rate_limit_initial_backoff: RATE_LIMIT_INITIAL_BACKOFF,
            rate_limit_max_backoff: RATE_LIMIT_MAX_BACKOFF,
            rate_limit_max_elapsed: RATE_LIMIT_MAX_ELAPSED,
            response_header_timeout: RESPONSE_HEADER_TIMEOUT,
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

    /// A config for a caller that must fail fast rather than eat the
    /// LLM-tuned retry ladder (aux fail-fast: narrate/session-title/
    /// summarize riding a pinned model, #560 follow-up). A dead endpoint —
    /// connection refused, or one that never answers — must cost this caller
    /// a bounded probe, not the ~1-minute-plus worst case of the 5-attempt
    /// default ladder. At most one retry on a genuine transport/5xx failure
    /// with a short backoff, a short response-header wait, and a tightly
    /// bounded 429 park. Admission is unaffected — this still queues behind
    /// the same endpoint limits as ordinary traffic, it just gives up quickly
    /// once admitted.
    pub fn minimal() -> Self {
        Self {
            max_attempts: 2,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(1),
            rate_limit_max_elapsed: Duration::from_secs(5),
            response_header_timeout: Duration::from_secs(10),
            ..Self::default()
        }
    }
}

/// Shared HTTP client + per-endpoint limiter pool. Cheap to clone: the
/// `reqwest::Client` is `Arc`-wrapped internally and the endpoint pool is shared
/// behind an `Arc`.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    pool: Arc<EndpointPool>,
}

/// The admission one request holds — its endpoint permit, its model permit
/// (when the model has a cap) and its cross-process lease (when sharing is
/// on) — for the whole request **and its streamed body**. Dropping it frees
/// the slots for the next caller or peer process.
pub struct StreamGuard(
    #[allow(dead_code)] Option<OwnedSemaphorePermit>,
    #[allow(dead_code)] Option<OwnedSemaphorePermit>,
    #[allow(dead_code)] Option<shared_state::SharedLease>,
);

impl HttpClient {
    /// Create a shared HTTP client with the default [`RetryConfig`].
    ///
    /// Fails if the underlying `reqwest::Client` builder fails (TLS backend
    /// init, resolver issues) — an unusual host-environment condition, not a
    /// bug, so it surfaces as an error rather than panicking the process
    /// (#583).
    pub fn new() -> anyhow::Result<Self> {
        Self::with_config(RetryConfig::default())
    }

    /// Create a shared HTTP client with a custom per-endpoint [`RetryConfig`],
    /// its cross-process endpoint state resolved from the environment
    /// ([`SharedStateLocation::from_env`]).
    ///
    /// See [`HttpClient::new`] for why this is fallible.
    pub fn with_config(config: RetryConfig) -> anyhow::Result<Self> {
        Self::with_shared_state(SharedStateLocation::from_env(), config)
    }

    /// Create a shared HTTP client whose cross-process endpoint state
    /// (ADR-0144) lives at an explicit `shared` location instead of the
    /// env-resolved one — a per-test temp dir, `Disabled`, or an embedder's
    /// own directory (#743).
    ///
    /// See [`HttpClient::new`] for why this is fallible.
    pub fn with_shared_state(
        shared: SharedStateLocation,
        config: RetryConfig,
    ) -> anyhow::Result<Self> {
        let client = client_builder()
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            client,
            pool: Arc::new(EndpointPool {
                endpoints: Mutex::new(HashMap::new()),
                config,
                shared,
                default_limits: RateLimits::DEFAULT,
            }),
        })
    }

    /// This client with `limits` as the fallback for every endpoint's unset
    /// axes instead of [`RateLimits::DEFAULT`] — for endpoints no catalog
    /// entry describes (MCP servers) or tests. Returns a client with a fresh,
    /// empty endpoint pool; clones of `self` keep the old one.
    pub fn with_default_limits(&self, limits: RateLimits) -> Self {
        Self {
            client: self.client.clone(),
            pool: Arc::new(EndpointPool {
                endpoints: Mutex::new(HashMap::new()),
                config: self.pool.config,
                shared: self.pool.shared.clone(),
                default_limits: limits,
            }),
        }
    }

    /// Get the underlying `reqwest::Client` for making requests.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

/// Retry error types.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("max retry attempts ({0}) exhausted: {1}")]
    Exhausted(u32, reqwest::Error),
    #[error("permanent error, not retrying: {0}")]
    Permanent(reqwest::Error),
    /// Gave up waiting for the endpoint's `Retry-After` cool-down (in-process
    /// or the cross-process shared gate) to clear within
    /// `rate_limit_max_elapsed`, without ever sending a request (#547) — the
    /// no-response counterpart to the 429-with-a-response give-up path,
    /// which still returns `Ok` with the 429 itself for the caller to
    /// inspect.
    #[error("rate limited: gave up waiting for the endpoint's retry-after cool-down to clear")]
    RateLimited,
    /// The server accepted the connection and the request but never wrote
    /// response headers within [`RESPONSE_HEADER_TIMEOUT`], on every attempt up
    /// to `max_attempts` (#658).
    #[error(
        "no response headers within {0:?} after {1} attempt(s): server accepted the request \
         but never replied"
    )]
    HeaderTimeout(Duration, u32),
}

#[cfg(test)]
mod tests;

/// A pool client with cross-process endpoint state disabled (#743): a unit
/// test must never touch the developer's real endpoint-state dir. Tests that
/// exercise the shared gate itself inject a temp `SharedStateLocation::Dir`.
#[cfg(test)]
pub(crate) fn test_http_client() -> HttpClient {
    test_http_client_with(RetryConfig::default())
}

/// [`test_http_client`] with a custom [`RetryConfig`].
#[cfg(test)]
pub(crate) fn test_http_client_with(config: RetryConfig) -> HttpClient {
    HttpClient::with_shared_state(SharedStateLocation::Disabled, config)
        .expect("reqwest client builds in tests")
}
