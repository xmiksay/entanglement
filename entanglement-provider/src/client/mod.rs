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

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::sleep;

mod status;
pub use status::ThrottleStatus;

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
/// POSTs. Overridable via `ENTANGLEMENT_MAX_CONCURRENCY`.
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

/// A held permit bounding the number of in-flight requests to one endpoint. It
/// is kept for the whole request **and its streamed body** (moved into the byte
/// pump), so the concurrency cap counts open streams — the real unit a provider
/// limits — not just POSTs. Dropping it frees a slot for a queued caller.
pub struct StreamGuard(#[allow(dead_code)] OwnedSemaphorePermit);

/// One endpoint's live rate-limit + backoff state.
struct EndpointState {
    limiter: RateLimiter,
    /// Bounds simultaneously in-flight requests to this endpoint. Cloned into a
    /// [`StreamGuard`] per request; the permit lives until the stream ends.
    concurrency: Arc<Semaphore>,
    /// The configured in-flight cap (initial permit count). Kept alongside the
    /// semaphore because `Semaphore` exposes only *available* permits, so a
    /// status reader needs this to compute in-flight = cap − available.
    concurrency_cap: usize,
    /// Instant before which no request to this endpoint may proceed, set from a
    /// `Retry-After` header. `None` = no active cool-down.
    retry_after: Mutex<Option<Instant>>,
}

impl EndpointState {
    fn new(rpm: u32, concurrency: usize) -> Self {
        let cap = concurrency.max(1);
        Self {
            limiter: RateLimiter::new(rpm),
            concurrency: Arc::new(Semaphore::new(cap)),
            concurrency_cap: cap,
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
struct RateLimiter {
    /// The at-rest spacing (`60s / rpm`) and the floor `relax` recovers to.
    base: Duration,
    /// The most-throttled spacing `penalize` will grow to (`base × SLOWDOWN_CAP`).
    max: Duration,
    state: Mutex<PaceState>,
}

struct PaceState {
    /// Current spacing between consecutive requests, in `[base, max]`.
    interval: Duration,
    /// Earliest instant the next request may proceed. Each `acquire` reserves
    /// this slot and advances it, so concurrent callers get distinct, spaced
    /// slots instead of all firing at once.
    next_slot: Instant,
}

impl RateLimiter {
    fn new(rpm: u32) -> Self {
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
    async fn acquire(&self) {
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
    fn penalize(&self) {
        let mut st = self.state.lock().expect("rate limiter poisoned");
        st.interval = (st.interval * 2).min(self.max);
    }

    /// A request to this endpoint succeeded: step the spacing back toward `base`
    /// by one base unit (additive increase of the effective rate). A no-op once
    /// already at `base`.
    fn relax(&self) {
        let mut st = self.state.lock().expect("rate limiter poisoned");
        st.interval = st.interval.saturating_sub(self.base).max(self.base);
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
            .connect_timeout(CONNECT_TIMEOUT)
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

    /// Resolve (creating on first use) the resilience state for pool key `key`.
    /// `rpm`/`concurrency` are the endpoint's catalog-provided budget/cap; `None`
    /// falls back to the pool's defaults (`RetryConfig::rpm`/`concurrency`). Only
    /// the *first* caller for a key sets the bucket size — an endpoint is one
    /// provider, so the value is consistent.
    fn endpoint(
        &self,
        key: &str,
        rpm: Option<u32>,
        concurrency: Option<usize>,
    ) -> Arc<EndpointState> {
        let rpm = rpm.unwrap_or(self.pool.config.rpm);
        let concurrency = concurrency.unwrap_or(self.pool.config.concurrency);
        let mut map = self.pool.endpoints.lock().expect("endpoint pool poisoned");
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(EndpointState::new(rpm, concurrency)))
            .clone()
    }

    /// Execute a request with per-endpoint rate-limiting and retry/backoff. The
    /// RPM budget + `Retry-After` window are keyed by `(endpoint, api_key)`: the
    /// provider's base URL **plus** the API key (if any), so multiple keys on the
    /// same endpoint each get their own budget — different keys have different
    /// limits (#217). `rpm`/`concurrency` are the endpoint's catalog-provided
    /// per-minute budget / in-flight cap (`None` → the pool defaults, #414).
    ///
    /// Returns the response **plus a [`StreamGuard`]** the caller must keep alive
    /// for the whole streamed body — it holds the per-endpoint concurrency permit
    /// (the storm guard for many spawned sub-agents). A **429 parks the whole
    /// endpoint and retries** on a growing wait until it clears *or*
    /// `rate_limit_max_elapsed` is exceeded, then surfaces as `Ok` for the caller
    /// to error (so a saturated endpoint fails a turn rather than hanging its
    /// parent). Transient transport faults and 5xx retry up to `max_attempts`; a
    /// permanent 4xx or an exhausted retryable is returned as `Ok`.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_with_retry<F, Fut>(
        &self,
        endpoint: &str,
        api_key: Option<&str>,
        rpm: Option<u32>,
        concurrency: Option<usize>,
        request_fn: F,
    ) -> Result<(reqwest::Response, StreamGuard), RetryError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let endpoint = self.endpoint(&pool_key(endpoint, api_key), rpm, concurrency);
        let config = self.pool.config;
        // `attempt` bounds only *genuine failures* (5xx / transport faults). A
        // 429 is "wait your turn": it retries until it clears (not counted here),
        // bounded overall by `rate_limit_max_elapsed`.
        let mut attempt = 0;
        let mut backoff = config.initial_backoff;
        let mut rl_backoff = config.rate_limit_initial_backoff;
        let rl_deadline = Instant::now() + config.rate_limit_max_elapsed;

        loop {
            endpoint.wait_for_retry_after().await;
            endpoint.limiter.acquire().await;
            // Bound in-flight requests to this endpoint. The permit is held until
            // the returned `StreamGuard` drops (i.e. the stream is consumed);
            // dropped here on any retry so a queued caller can take the slot.
            let permit = endpoint
                .concurrency
                .clone()
                .acquire_owned()
                .await
                .expect("endpoint concurrency semaphore never closed");

            match request_fn().await {
                Ok(response) => {
                    let status = response.status();
                    // Success: recover the endpoint's pacing a notch, hand back
                    // the response together with the held concurrency permit.
                    if status.is_success() {
                        endpoint.limiter.relax();
                        return Ok((response, StreamGuard(permit)));
                    }
                    // Permanent 4xx: hand it straight back — the caller inspects
                    // `!is_success()`. (Permit drops as the guard is dropped by
                    // the caller after reading the body.)
                    if !is_retryable_status(status) {
                        return Ok((response, StreamGuard(permit)));
                    }
                    let retry_after = parse_retry_after(response.headers());

                    // 429 "too many requests": release the in-flight slot, park
                    // the *whole* endpoint (every concurrent caller backs off
                    // together), slow the pacing gate, and retry on a growing wait
                    // — until it clears or the overall `rate_limit_max_elapsed`
                    // budget is spent, at which point surface it as an error.
                    if status.as_u16() == 429 {
                        endpoint.limiter.penalize();
                        let delay = retry_after
                            .unwrap_or_else(|| rl_backoff.min(config.rate_limit_max_backoff));
                        endpoint.set_retry_after(delay);
                        // Give up once another wait would exceed the overall
                        // budget: surface the 429 (permit still held) so the
                        // caller errors instead of the parent hanging forever.
                        if Instant::now() + delay >= rl_deadline {
                            tracing::error!(
                                status = %status,
                                "rate limited (429): giving up after exhausting the retry budget"
                            );
                            return Ok((response, StreamGuard(permit)));
                        }
                        drop(permit); // free the slot while we back off
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
                    // server advised a `Retry-After`.
                    attempt += 1;
                    if let Some(server_delay) = retry_after {
                        endpoint.set_retry_after(server_delay);
                    }
                    if attempt >= config.max_attempts {
                        return Ok((response, StreamGuard(permit)));
                    }
                    drop(permit);
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
                Err(e) if !is_transient_error(&e) => return Err(RetryError::Permanent(e)),
                Err(e) => {
                    drop(permit);
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

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Forward a streaming response body over an mpsc channel, chunk by chunk, with
/// an **idle-gap** watchdog: if no bytes arrive within [`STREAM_IDLE_TIMEOUT`]
/// the stream is aborted with an error frame. This replaces the old
/// whole-request timeout — a long healthy SSE stream now runs to completion
/// while a hung one still dies fast (#241). `label` names the source (e.g.
/// `"openai-compat"`) for the error messages. The `reqwest::Client` is built
/// with `connect_timeout` only, so this per-chunk gap is what bounds a stalled
/// body.
///
/// The [`StreamGuard`] (the per-endpoint concurrency permit from
/// [`HttpClient::execute_with_retry`]) is moved into the pump task and dropped
/// when the body ends, so an endpoint's in-flight cap counts open streams for
/// their full lifetime — not just until the response headers arrive.
pub fn spawn_byte_stream(
    response: reqwest::Response,
    label: &'static str,
    guard: StreamGuard,
) -> mpsc::Receiver<Result<Vec<u8>, anyhow::Error>> {
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, anyhow::Error>>(8);
    tokio::spawn(async move {
        let _guard = guard; // release the concurrency slot when the body ends
        let mut bytes = response.bytes_stream();
        loop {
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, bytes.next()).await {
                Ok(Some(item)) => {
                    let chunk = item
                        .map(|c| c.to_vec())
                        .map_err(|e| anyhow::anyhow!("{label} stream read: {e}"));
                    if tx.send(chunk).await.is_err() {
                        break; // consumer dropped
                    }
                }
                Ok(None) => break, // stream ended cleanly
                Err(_) => {
                    let _ = tx
                        .send(Err(anyhow::anyhow!(
                            "{label} stream stalled: no data for {STREAM_IDLE_TIMEOUT:?}"
                        )))
                        .await;
                    break;
                }
            }
        }
    });
    rx
}

/// The pool identity for a request: the endpoint URL, plus a **hash** of the API
/// key when one is present, so two keys on the same endpoint get independent
/// rate-limit budgets (#217). The key is hashed, never stored raw — the map key
/// must not carry the secret. The hash is process-local (bucket partitioning
/// only), so cross-run stability is irrelevant.
fn pool_key(endpoint: &str, api_key: Option<&str>) -> String {
    match api_key {
        Some(key) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut hasher);
            format!("{endpoint}#{:016x}", hasher.finish())
        }
        None => endpoint.to_string(),
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
    // Connection establishment, timeout, or a request-send fault. A dropped
    // keep-alive connection reset between requests renders as `is_request()`
    // (reqwest's "error sending request for url ...") — not `is_connect()`,
    // which flags connection *establishment* only. Retrying is safe: the body
    // was sent up front and no response body was consumed, so a resend is a
    // fresh attempt, not a partial-write hazard.
    if error.is_timeout() || error.is_connect() || error.is_request() {
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

/// Env flag that opts into logging full LLM request bodies. A request body
/// carries the system prompt, the **entire conversation**, and the tool schemas
/// — repo/user data (never API keys: those ride in headers). It is therefore off
/// by default and only ever emitted when a human explicitly asks for it; `RUST_LOG`
/// verbosity alone is deliberately not enough (#165).
const LOG_BODIES_ENV: &str = "ENTANGLEMENT_LOG_BODIES";

/// Cap on how many bytes of a request body ever reach the log, even opted in —
/// a full conversation can be megabytes; this keeps the sink bounded.
const MAX_LOGGED_BODY_BYTES: usize = 8 * 1024;

/// Log an LLM request body at `debug!`, but **only** when
/// `ENTANGLEMENT_LOG_BODIES=1`. Shared by every provider client so body logging
/// is symmetric and greppable (`provider` tags the backend); the payload holds
/// conversation + repo data, so it stays behind the explicit opt-in and is
/// truncated to [`MAX_LOGGED_BODY_BYTES`] (#165).
pub fn log_request_body(provider: &str, body: &serde_json::Value) {
    if std::env::var(LOG_BODIES_ENV).as_deref() != Ok("1") {
        return;
    }
    let rendered = body.to_string();
    let (shown, truncated) = truncate_on_boundary(&rendered, MAX_LOGGED_BODY_BYTES);
    tracing::debug!(
        provider,
        bytes = rendered.len(),
        truncated,
        body = shown,
        "LLM request body (conversation + repo data; gated by ENTANGLEMENT_LOG_BODIES=1)"
    );
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 char, returning
/// the slice and whether anything was dropped.
fn truncate_on_boundary(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
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
mod tests;
