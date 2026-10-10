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
//! Retries transient failures — connect/timeout faults, request-send faults (a
//! dropped keep-alive connection reset between requests), dropped streams, and
//! retryable 5xx — with exponential backoff + jitter, bounded by `max_attempts`.
//! A **429 is treated as "wait your turn", not a failure**: it is retried
//! *until it clears* on its own patient schedule (≈5s ramping to ≈10 min), never
//! consuming the failure budget and never surfaced as an error. Before #217 a
//! 429/5xx *response* came back as `reqwest::Ok` and so was never retried (#193):
//! the classification happens on the `Response`, not just on `reqwest::Error`.
//!
//! # Rate-limit handling (per endpoint)
//! Each endpoint owns a **concurrency cap**, an **adaptive pacing gate**, and a
//! `Retry-After` window — all shared across every session/backend clone that
//! talks to the same `(base URL, api-key)`, so throttling is a property of the
//! endpoint, not of any one session. The concurrency cap (default 3, held for
//! the whole request *and its streamed body*) is the primary guard: many spawned
//! sub-agents queue and run a few at a time instead of all firing at once and
//! 429-storming a provider's real concurrency ceiling. A 429 (with or without
//! `Retry-After`) then parks every caller of *that* endpoint and slows the pacing
//! gate; successes speed it back up. One throttled endpoint never blocks another.
//! This is why spawning many sessions can't leave "one running, the rest 429" —
//! or hang the parent: they all meter through one gate, and a 429 that never
//! clears within the budget surfaces as an error rather than waiting forever.
//!
//! # Per-model concurrency (#521, ADR-0140)
//! Some providers cap concurrency **per model**, not just per endpoint — z.ai
//! allows only 1 in-flight `GLM-4.7-Flash` request but 5 for `GLM-5.2`, on the
//! same base URL and key. The endpoint-wide cap stays as the ceiling on the
//! **sum** of in-flight requests across every model sharing it; when a model
//! carries its own catalog `concurrency` cap, a request also acquires a second
//! permit from that model's own semaphore, scoped to this endpoint
//! (`EndpointState::model_concurrency`) and never allowed to admit more than
//! its own cap regardless of how much endpoint headroom exists. Permits
//! acquire **model first, then endpoint** — a caller blocked on its own
//! model's slot must never hold the scarce endpoint slot hostage, which would
//! starve sibling models sharing the endpoint — and release in the reverse
//! order. Both are held for the whole streamed body via `StreamGuard`. The
//! endpoint-wide `Retry-After`/pacing stay shared across every model on it (a
//! 429 may well be host-wide); a model carrying no cap admits solely through
//! the endpoint gate, unchanged from before this layer existed. The cap
//! passed in is resolved by the caller **per request**, against the
//! request's own model rather than baked in once at client construction
//! (#550, `ModelConcurrencyResolver`) — and `EndpointState::model_slot`
//! corrects an already-cached slot's cap if a later caller supplies a
//! different one, rather than latching the first value seen for the rest of
//! the process (ADR-0140's whole point is a *correct* per-model cap; a
//! silently-wrong one that can never self-heal defeats it).
//!
//! # Shared cross-process lease ordering (#546)
//! The cross-process gate (below) is scarcer still than either in-process
//! permit — it is capped process-*wide*, across every `skutter` sharing the
//! endpoint+key. It is therefore acquired **last**, only once both the model
//! and endpoint permits are already held: a caller queued on its own model's
//! semaphore (which can block for a full lease-renewal interval, ~60s) must
//! never sit holding the shared lease while it waits, or it starves sibling
//! processes even though the provider sees room. Acquiring it only right
//! before the request fires also keeps the shared RPM ledger's timestamp
//! (stamped on admission) meaning what it says — the start of the send, not
//! the start of a possibly long in-process queue wait.
//!
//! # Per-user admission gate (#632, ADR-0175)
//! Two users sharing one *literal* API key land in the same `EndpointState`
//! (ADR-0050's pool keys on `(base_url, sha256(api_key))`, blind to who's
//! calling), so per-user budgets need a gate layered on top rather than
//! falling out of the pool key. `user_budget.rs` adds exactly that — a
//! `UserSlot` mirroring `ModelSlot`, keyed by `UserId` instead of model id,
//! resolved from that user's own catalog rpm/concurrency and attached to a
//! `HttpClient` handle via `with_user_budget`. Acquired **first**, before the
//! model/endpoint permits: the narrowest, most caller-specific gate goes
//! first so it never holds a wider one hostage. A handle with no attached
//! budget (every existing single-user caller) admits exactly as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::sync::OwnedSemaphorePermit;

mod backoff;
mod endpoint;
mod rate_limiter;
mod retry;
mod shared_state;
mod shared_store;
mod status;
mod stream;
mod tls;
mod user_budget;
pub use backoff::extract_retry_after_from_response;
pub use shared_state::{prune_stale, SharedStateLocation};
pub use status::ThrottleStatus;
pub use stream::{log_request_body, spawn_byte_stream};
pub use tls::{client_builder, install_default_crypto_provider};
pub use user_budget::UserBudget;

use endpoint::EndpointPool;

const MAX_RETRY_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_HOST: usize = 10;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const RPM_LIMIT: u32 = 50;

/// Default cap on **simultaneously in-flight requests to one endpoint** — the
/// primary guard against a spawn-storm (many sub-agents) overrunning a
/// provider's concurrency ceiling and 429-storming. A permit is held for the
/// whole request *and its streamed body*, so this counts open streams, not just
/// POSTs. A provider sets its own via the catalog `concurrency:` (ADR-0213).
const DEFAULT_CONCURRENCY: usize = 3;

/// Total wall-clock a single 429 will keep retrying before it gives up and
/// surfaces the response as an error. A concurrency-capped request should rarely
/// hit this; it exists so a genuinely saturated endpoint fails a sub-agent's
/// turn (returning a failed `ToolResult`) instead of hanging its parent forever.
const RATE_LIMIT_MAX_ELAPSED: Duration = Duration::from_secs(900);

/// Under sustained 429s the per-endpoint request spacing is slowed
/// multiplicatively down to at most this multiple of the base interval — i.e.
/// the effective RPM floor is `rpm / SLOWDOWN_CAP`. Bounds how far the pacing
/// gate will back off before it stops slowing and leans on the `Retry-After`
/// window instead.
const SLOWDOWN_CAP: u32 = 8;

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

/// Retry/rate-limit tuning applied per endpoint. Defaults match the historical
/// shared client (5 attempts, 200ms→30s backoff, 50 RPM) — now *per endpoint*
/// rather than global. `max_attempts`/`initial_backoff`/`max_backoff` bound the
/// *failure* path (5xx, transport faults); `rate_limit_*` is the separate,
/// unbounded 429 schedule (ADR-0111).
#[derive(Clone, Copy, Debug)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Requests per minute allowed to each endpoint before throttling.
    pub rpm: u32,
    /// Max simultaneously in-flight requests **per endpoint** (held across the
    /// streamed body). The primary storm guard for many spawned sub-agents.
    pub concurrency: usize,
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
            rpm: RPM_LIMIT,
            concurrency: DEFAULT_CONCURRENCY,
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
    /// bounded 429 park. The RPM/concurrency/shared-lease admission gates are
    /// unaffected (`execute_with_retry`'s `retry` override only ever changes
    /// the failure-path knobs) — this still queues behind the same endpoint
    /// budget as ordinary traffic, it just gives up quickly once admitted.
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

/// Shared HTTP client + per-endpoint resilience pool. Cheap to clone: the
/// `reqwest::Client` is `Arc`-wrapped internally and the endpoint pool is shared
/// behind an `Arc`.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    pool: Arc<EndpointPool>,
    /// This handle's per-user admission gate (#632, ADR-0175); `None` for
    /// every existing single-user caller. Attached via `with_user_budget`,
    /// not a constructor parameter, so `HttpClient::new`/`with_config` stay
    /// byte-identical.
    user_budget: Option<Arc<UserBudget>>,
}

/// A held permit bounding the number of in-flight requests to one endpoint —
/// and, when the requested model carries its own tighter cap (#521), also one
/// bounding in-flight requests to that model on this endpoint — and, when the
/// calling handle carries a per-user budget (#632, ADR-0175), one bounding
/// in-flight requests for that user on this endpoint — plus, when the
/// cross-process gate is active (#523, ADR-0144), the shared lease bounding
/// in-flight requests across every `skutter` process sharing this endpoint.
/// All are kept for the whole request **and its streamed body** (moved into
/// the byte pump), so the concurrency cap counts open streams — the real unit
/// a provider limits — not just POSTs. Dropping it frees a slot for a queued
/// caller (in-process) or peer process (cross-process).
pub struct StreamGuard(
    #[allow(dead_code)] OwnedSemaphorePermit,
    #[allow(dead_code)] Option<OwnedSemaphorePermit>,
    #[allow(dead_code)] Option<OwnedSemaphorePermit>,
    #[allow(dead_code)] Option<shared_state::SharedLease>,
);

impl HttpClient {
    /// Create a shared HTTP client with default per-endpoint retry/rate-limit.
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
            }),
            user_budget: None,
        })
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
