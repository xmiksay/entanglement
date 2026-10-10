//! Per-endpoint resilience state: the pool keyed by normalized URL + API-key
//! hash, the endpoint/model concurrency gates and the `Retry-After` window.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::time::sleep;

use super::rate_limiter::RateLimiter;
#[cfg(doc)]
use super::StreamGuard;
use super::{shared_state, user_budget, HttpClient, RetryConfig};
use crate::UserId;

/// Per-endpoint resilience state, lazily created on first use and keyed by the
/// endpoint's base URL. Each endpoint carries its own RPM budget + `Retry-After`
/// window so throttling on one API never blocks another.
pub(super) struct EndpointPool {
    pub(super) endpoints: Mutex<HashMap<String, Arc<EndpointState>>>,
    pub(super) config: RetryConfig,
}

/// RAII bump of an [`EndpointState::waiters`] counter — display-only queue
/// depth (#552, orig. #517). Decrements on `Drop` rather than
/// after a plain `await` so a caller cancelled mid-wait (e.g. `Stop` dropping
/// the whole `execute_with_retry` future) still releases its count instead of
/// leaking it forever.
pub(super) struct WaiterGuard<'a>(&'a AtomicUsize);

impl<'a> WaiterGuard<'a> {
    pub(super) fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One model's live in-flight cap on a given endpoint (#521, ADR-0140) — a
/// second, tighter admission gate layered *underneath* the endpoint-wide
/// [`EndpointState::concurrency`] permit. z.ai enforces concurrency per model
/// (e.g. GLM-4.7-Flash: 1, GLM-5.2: 5) rather than only per endpoint, so one
/// endpoint-wide cap can't express a mixed-model workload without either
/// 429-storming the tighter model or needlessly serializing the looser one.
pub(super) struct ModelSlot {
    pub(super) semaphore: Arc<Semaphore>,
    /// The configured cap (initial permit count) — see `EndpointState::
    /// concurrency_cap`'s doc for why this is tracked alongside the semaphore.
    pub(super) cap: usize,
}

impl ModelSlot {
    pub(super) fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            semaphore: Arc::new(Semaphore::new(cap)),
            cap,
        }
    }
}

/// One endpoint's live rate-limit + backoff state.
pub(super) struct EndpointState {
    pub(super) limiter: RateLimiter,
    /// Bounds simultaneously in-flight requests to this endpoint. Cloned into a
    /// [`StreamGuard`] per request; the permit lives until the stream ends.
    pub(super) concurrency: Arc<Semaphore>,
    /// The configured in-flight cap (initial permit count). Kept alongside the
    /// semaphore because `Semaphore` exposes only *available* permits, so a
    /// status reader needs this to compute in-flight = cap − available.
    pub(super) concurrency_cap: usize,
    /// Count of callers currently queued on [`concurrency`][Self::concurrency]'s
    /// `acquire_owned()` — incremented just before the call, decremented right
    /// after (#517, #552), purely for display via
    /// `status()`; nothing on the request path reads it.
    pub(super) waiters: AtomicUsize,
    /// Instant before which no request to this endpoint may proceed, set from a
    /// `Retry-After` header. `None` = no active cool-down.
    pub(super) retry_after: Mutex<Option<Instant>>,
    /// Per-model in-flight caps on this endpoint (#521), lazily created on
    /// first use like `EndpointPool::endpoints` itself — keyed by model id,
    /// scoped to this one endpoint since a per-model cap is meaningless
    /// detached from the provider that enforces it.
    pub(super) model_concurrency: Mutex<HashMap<String, Arc<ModelSlot>>>,
    /// Per-user rpm/concurrency slots on this endpoint (#632, ADR-0175),
    /// lazily created like `model_concurrency` — keyed by `UserId`, resolved
    /// only when the calling `HttpClient` handle carries a `UserBudget`.
    pub(super) user_budgets: Mutex<HashMap<UserId, Arc<user_budget::UserSlot>>>,
    /// This endpoint's cross-process gate (#523, ADR-0144) — file-backed,
    /// shared RPM/concurrency/cool-down state so sibling `skutter` processes
    /// talking to the same endpoint+key collectively respect one budget
    /// instead of each applying it in full.
    pub(super) shared: shared_state::SharedGate,
    /// The resolved RPM this endpoint was created with, re-passed to
    /// `shared` on every acquisition (the shared gate has no other way to
    /// learn it — it is keyed by file path, not by config).
    pub(super) rpm: u32,
}

impl EndpointState {
    pub(super) fn new(rpm: u32, concurrency: usize, pool_key: &str) -> Self {
        let cap = concurrency.max(1);
        Self {
            limiter: RateLimiter::new(rpm),
            concurrency: Arc::new(Semaphore::new(cap)),
            concurrency_cap: cap,
            waiters: AtomicUsize::new(0),
            retry_after: Mutex::new(None),
            model_concurrency: Mutex::new(HashMap::new()),
            user_budgets: Mutex::new(HashMap::new()),
            shared: shared_state::SharedGate::new(pool_key),
            rpm,
        }
    }

    /// Extend (never shorten) this endpoint's cool-down window by `delay`.
    pub(super) fn set_retry_after(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut guard = self.retry_after.lock().expect("retry_after poisoned");
        if guard.is_none_or(|cur| until > cur) {
            *guard = Some(until);
        }
    }

    /// Park until any active `Retry-After` window has elapsed, bounded by
    /// `deadline` — the caller's own `rate_limit_max_elapsed` budget (#547).
    /// The endpoint-wide cool-down this reads is itself clamped to
    /// `rate_limit_max_backoff` at the point it's set, but a value written by
    /// an older build, a sibling process on different config, or read back
    /// from the cross-process file after a restart could still exceed one
    /// caller's own deadline — this is the backstop that keeps *this* caller
    /// from ever sleeping past its own budget regardless of who set the
    /// cool-down or why. `Err(())` means the wait would need to extend past
    /// `deadline`: the caller gives up without sleeping further.
    pub(super) async fn wait_for_retry_after(&self, deadline: Instant) -> Result<(), ()> {
        let until = *self.retry_after.lock().expect("retry_after poisoned");
        if let Some(until) = until {
            if until >= deadline {
                return Err(());
            }
            let now = Instant::now();
            if until > now {
                sleep(until - now).await;
            }
        }
        Ok(())
    }

    /// Resolve (creating on first use) this endpoint's slot for `model`. Unlike
    /// `HttpClient::endpoint`'s "first caller wins" endpoint sizing, a later
    /// caller supplying a *different* cap **corrects** the cached slot rather
    /// than being ignored (#550): the per-request [`ModelConcurrencyResolver`]
    /// resolves the right model's cap on every call now, but a stale/wrong
    /// value could still reach here first (a config reload, or simply the
    /// very first caller racing ahead of a correction) — and with the old
    /// first-caller-wins semantics that mistake stuck for the rest of the
    /// process, exactly the "permanent wrong cap" failure ADR-0140 exists to
    /// prevent. Replacing the map entry is safe: any permit already checked
    /// out against the old `Arc<Semaphore>` remains valid and simply releases
    /// into a semaphore no longer referenced by new callers, who all pick up
    /// the corrected one instead. A model cap wider than the endpoint's own is
    /// legal — the endpoint cap simply binds first as the ceiling on the sum
    /// across every model — but is almost certainly a misconfiguration (the
    /// model cap can then never be the binding constraint), so it warns rather
    /// than errors.
    pub(super) fn model_slot(&self, model: &str, cap: usize) -> Arc<ModelSlot> {
        let mut map = self
            .model_concurrency
            .lock()
            .expect("model concurrency poisoned");
        if let Some(slot) = map.get(model) {
            if slot.cap == cap {
                return slot.clone();
            }
            tracing::warn!(
                model,
                previous_cap = slot.cap,
                corrected_cap = cap,
                "model concurrency cap changed for this model — correcting the \
                 cached slot instead of keeping the stale cap for the rest of \
                 the process"
            );
        }
        if cap > self.concurrency_cap {
            tracing::warn!(
                model,
                model_concurrency = cap,
                endpoint_concurrency = self.concurrency_cap,
                "model concurrency cap exceeds the endpoint's own — \
                 the endpoint cap binds first, so this model can never \
                 reach its configured cap"
            );
        }
        let slot = Arc::new(ModelSlot::new(cap));
        map.insert(model.to_string(), slot.clone());
        slot
    }
}

impl HttpClient {
    /// Resolve (creating on first use) the resilience state for pool key `key`.
    /// `rpm`/`concurrency` are the endpoint's catalog-provided budget/cap; `None`
    /// falls back to the pool's defaults (`RetryConfig::rpm`/`concurrency`). Only
    /// the *first* caller for a key sets the bucket size — an endpoint is one
    /// provider, so the value is consistent.
    pub(super) fn endpoint(
        &self,
        key: &str,
        rpm: Option<u32>,
        concurrency: Option<usize>,
    ) -> Arc<EndpointState> {
        let rpm = rpm.unwrap_or(self.pool.config.rpm);
        let concurrency = concurrency.unwrap_or(self.pool.config.concurrency);
        let mut map = self.pool.endpoints.lock().expect("endpoint pool poisoned");
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(EndpointState::new(rpm, concurrency, key)))
            .clone()
    }
}

/// The pool identity for a request: the [`normalize_endpoint`]d URL, plus a
/// **hash** of the API key when one is present, so two keys on the same
/// endpoint get independent rate-limit budgets (#217). The key is hashed,
/// never stored raw — the map key must not carry the secret. This value also
/// becomes the cross-process shared-state **file name**
/// (`shared_store::hash_key`, #523) — so, unlike before #551, it must be
/// stable across processes: `DefaultHasher`'s output is documented
/// unspecified and varies by Rust toolchain/version, which silently split two
/// `skutter` binaries built differently onto different shared-state files for
/// the same real endpoint. `sha2` is already a dependency (`shared_store`
/// uses it for the same reason), so it costs nothing extra to use here too.
pub(super) fn pool_key(endpoint: &str, api_key: Option<&str>) -> String {
    let endpoint = normalize_endpoint(endpoint);
    match api_key {
        Some(key) => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(key.as_bytes());
            format!("{endpoint}#{:x}", hasher.finalize())
        }
        None => endpoint,
    }
}

/// Normalize an endpoint URL for pool-key identity (#551): trim trailing
/// `/` and lowercase the host so `https://Api.z.ai/v4/` and
/// `https://api.z.ai/v4` collapse onto the same [`EndpointState`] — and the
/// same shared-state file — instead of splitting one real endpoint's
/// RPM/concurrency budget into two independently-throttled halves. Every
/// wire client already trims its *own* request URL this way (`openai::mod`,
/// `gemini`) but historically keyed the pool on the raw, untrimmed field;
/// normalizing once here, centrally, means callers no longer need to agree
/// on doing it themselves. The host is lowercased (hostnames are
/// case-insensitive per RFC 3986); the path segment is left alone, since
/// paths are case-sensitive.
pub(super) fn normalize_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    match trimmed.find("://") {
        Some(scheme_end) => {
            let after_scheme = scheme_end + 3;
            let rest = &trimmed[after_scheme..];
            let host_end = rest.find('/').unwrap_or(rest.len());
            format!(
                "{}{}{}",
                &trimmed[..after_scheme],
                rest[..host_end].to_ascii_lowercase(),
                &rest[host_end..]
            )
        }
        None => trimmed.to_ascii_lowercase(),
    }
}
