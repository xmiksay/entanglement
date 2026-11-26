//! The one rate/concurrency limiter (#918).
//!
//! Every LLM request admits through one [`Limiter`] per endpoint (pool key =
//! normalized base URL + API-key hash). It enforces a [`RateLimits`] — the
//! single config shape, read from a catalog entry's `limits:` — on two keys:
//! the endpoint itself, and optionally the requested model on that endpoint
//! (z.ai caps concurrency per model). Each key is one [`Gate`]: a sliding
//! 60 s window over requests (`rpm`) and estimated tokens (`tpm`), plus a
//! semaphore over in-flight requests (`concurrency`). The endpoint's
//! `Retry-After` cool-down lives here too, so the retry loop and the
//! admission path read one state.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::sleep;

use super::endpoint::WaiterGuard;
use super::{shared_state, StreamGuard};

/// The sliding window `rpm`/`tpm` count over.
const WINDOW: Duration = Duration::from_secs(60);

/// One set of limits — the catalog's `limits:` block on a provider (the
/// endpoint) or on a model (that model on its provider's endpoint). `None`
/// leaves an axis unlimited on a model; on an endpoint it falls back to the
/// client's defaults ([`RateLimits::DEFAULT`] unless the embedder set its own
/// via `HttpClient::with_default_limits`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimits {
    /// Requests per minute.
    #[serde(default)]
    pub rpm: Option<u32>,
    /// Tokens per minute, counted from each request's estimated input size
    /// (body bytes / 4) — output tokens are unknown at admission.
    #[serde(default)]
    pub tpm: Option<u32>,
    /// Requests in flight at once (held until the streamed body ends).
    #[serde(default)]
    pub concurrency: Option<usize>,
}

impl RateLimits {
    /// The built-in endpoint fallback: 50 rpm and 3 in flight (held across the
    /// streamed body, so it counts open streams — the unit a provider limits —
    /// and guards against a sub-agent spawn storm); no `tpm`.
    pub const DEFAULT: Self = Self {
        rpm: Some(50),
        tpm: None,
        concurrency: Some(3),
    };

    /// This set with every unset axis taken from `fallback`.
    pub fn or(self, fallback: Self) -> Self {
        Self {
            rpm: self.rpm.or(fallback.rpm),
            tpm: self.tpm.or(fallback.tpm),
            concurrency: self.concurrency.or(fallback.concurrency),
        }
    }
}

/// The limits one request admits under: its endpoint's and its model's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestLimits {
    pub endpoint: RateLimits,
    pub model: RateLimits,
}

impl RequestLimits {
    /// `body`'s estimated input tokens (serialized bytes / 4) for a `tpm`
    /// window — 0, without serializing, when neither key counts tokens.
    pub(crate) fn estimate_tokens(&self, body: &impl serde::Serialize) -> u32 {
        if self.endpoint.tpm.is_none() && self.model.tpm.is_none() {
            return 0;
        }
        serde_json::to_vec(body).map_or(0, |bytes| {
            u32::try_from(bytes.len() / 4).unwrap_or(u32::MAX)
        })
    }
}

/// How long until a request costing `tokens` fits in the window `sent`
/// (`(admitted at, tokens)` pairs, oldest first), or `None` if it fits now.
/// Prunes entries older than [`WINDOW`] as a side effect. A single request
/// larger than the whole `tpm` still goes once the window is empty —
/// otherwise it could never be sent.
fn window_wait(
    sent: &mut VecDeque<(Instant, u32)>,
    limits: &RateLimits,
    now: Instant,
    tokens: u32,
) -> Option<Duration> {
    while sent
        .front()
        .is_some_and(|(at, _)| now.duration_since(*at) >= WINDOW)
    {
        sent.pop_front();
    }
    let requests_full = limits
        .rpm
        .is_some_and(|rpm| sent.len() >= rpm.max(1) as usize);
    let used: u64 = sent.iter().map(|(_, t)| u64::from(*t)).sum();
    let tokens_full = limits
        .tpm
        .is_some_and(|tpm| !sent.is_empty() && used + u64::from(tokens) > u64::from(tpm.max(1)));
    if !(requests_full || tokens_full) {
        return None;
    }
    // The oldest entry expiring is the earliest the window can change.
    sent.front()
        .map(|(at, _)| (*at + WINDOW).saturating_duration_since(now))
}

/// One key's live limits: the window and the in-flight semaphore.
pub(super) struct Gate {
    pub(super) limits: RateLimits,
    sent: Mutex<VecDeque<(Instant, u32)>>,
    semaphore: Option<Arc<Semaphore>>,
}

impl Gate {
    pub(super) fn new(limits: RateLimits) -> Self {
        Self {
            limits,
            sent: Mutex::new(VecDeque::new()),
            semaphore: limits
                .concurrency
                .map(|cap| Arc::new(Semaphore::new(cap.max(1)))),
        }
    }

    /// The configured in-flight cap, `None` when this key has none.
    pub(super) fn cap(&self) -> Option<usize> {
        self.limits.concurrency.map(|cap| cap.max(1))
    }

    /// Requests currently holding one of this key's permits.
    pub(super) fn in_flight(&self) -> usize {
        match (&self.semaphore, self.cap()) {
            (Some(sem), Some(cap)) => cap.saturating_sub(sem.available_permits()),
            _ => 0,
        }
    }

    /// Time until the window admits another request, `None` when it has room.
    pub(super) fn window_full_for(&self) -> Option<Duration> {
        let mut sent = self.sent.lock().expect("limiter window poisoned");
        window_wait(&mut sent, &self.limits, Instant::now(), 0)
    }

    /// Wait for a free in-flight slot (none to wait for without a cap).
    async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        let sem = self.semaphore.clone()?;
        Some(
            sem.acquire_owned()
                .await
                .expect("limiter semaphore is never closed"),
        )
    }

    /// Wait until the window has room for `tokens`, then record the request.
    /// The lock is held only to check and record, never across the sleep.
    async fn pace(&self, tokens: u32) {
        if self.limits.rpm.is_none() && self.limits.tpm.is_none() {
            return;
        }
        loop {
            let wait = {
                let mut sent = self.sent.lock().expect("limiter window poisoned");
                let now = Instant::now();
                match window_wait(&mut sent, &self.limits, now, tokens) {
                    None => {
                        sent.push_back((now, tokens));
                        return;
                    }
                    Some(wait) => wait,
                }
            };
            sleep(wait).await;
        }
    }
}

/// One endpoint's limiter: its own gate, a gate per capped model, the
/// `Retry-After` cool-down, and the cross-process lease file.
pub(super) struct Limiter {
    /// The client's fallback for unset endpoint axes.
    defaults: RateLimits,
    endpoint: Mutex<Arc<Gate>>,
    pub(super) models: Mutex<HashMap<String, Arc<Gate>>>,
    /// Callers queued on the endpoint permit — display only.
    pub(super) waiters: AtomicUsize,
    /// Instant before which no request to this endpoint may start.
    retry_after: Mutex<Option<Instant>>,
    pub(super) shared: shared_state::SharedGate,
}

impl Limiter {
    pub(super) fn new(
        limits: RateLimits,
        defaults: RateLimits,
        pool_key: &str,
        shared: &shared_state::SharedStateLocation,
    ) -> Self {
        Self {
            defaults,
            endpoint: Mutex::new(Arc::new(Gate::new(limits.or(defaults)))),
            models: Mutex::new(HashMap::new()),
            waiters: AtomicUsize::new(0),
            retry_after: Mutex::new(None),
            shared: shared_state::SharedGate::new(pool_key, shared),
        }
    }

    /// The endpoint gate for `limits`. A caller bringing different limits
    /// (an admin edited the catalog) replaces the gate rather than being
    /// silently bound by whoever came first; permits already out stay valid.
    pub(super) fn endpoint_gate(&self, limits: RateLimits) -> Arc<Gate> {
        let limits = limits.or(self.defaults);
        let mut gate = self.endpoint.lock().expect("endpoint gate poisoned");
        if gate.limits != limits {
            tracing::warn!(previous = ?gate.limits, corrected = ?limits, "endpoint limits changed");
            *gate = Arc::new(Gate::new(limits));
        }
        gate.clone()
    }

    /// The current endpoint gate, for status display.
    pub(super) fn current_endpoint_gate(&self) -> Arc<Gate> {
        self.endpoint
            .lock()
            .expect("endpoint gate poisoned")
            .clone()
    }

    /// The gate for `model` on this endpoint, `None` when the model carries
    /// no limits of its own. Corrected like [`endpoint_gate`][Self::endpoint_gate].
    pub(super) fn model_gate(&self, model: &str, limits: RateLimits) -> Option<Arc<Gate>> {
        if limits == RateLimits::default() {
            return None;
        }
        let mut models = self.models.lock().expect("model gates poisoned");
        if let Some(gate) = models.get(model) {
            if gate.limits == limits {
                return Some(gate.clone());
            }
            tracing::warn!(model, previous = ?gate.limits, corrected = ?limits, "model limits changed");
        }
        let gate = Arc::new(Gate::new(limits));
        models.insert(model.to_string(), gate.clone());
        Some(gate)
    }

    /// Admit one request: model permit and window, then endpoint permit and
    /// window, then the cross-process lease. Narrowest first, so a caller
    /// blocked on its own model never holds the endpoint slot (or a peer
    /// process's lease) hostage. `Err(())`: the cross-process gate would
    /// need to wait past `deadline`.
    pub(super) async fn admit(
        &self,
        model: &str,
        limits: RequestLimits,
        tokens: u32,
        deadline: Instant,
    ) -> Result<StreamGuard, ()> {
        let endpoint = self.endpoint_gate(limits.endpoint);
        let model_gate = self.model_gate(model, limits.model);
        let model_permit = match &model_gate {
            Some(gate) => {
                let permit = gate.acquire().await;
                gate.pace(tokens).await;
                permit
            }
            None => None,
        };
        let waiter = WaiterGuard::new(&self.waiters);
        let endpoint_permit = endpoint.acquire().await;
        drop(waiter);
        endpoint.pace(tokens).await;
        let lease = self
            .shared
            .acquire(
                endpoint.limits.rpm.unwrap_or(u32::MAX),
                endpoint.cap().unwrap_or(usize::MAX),
                deadline,
            )
            .await?;
        Ok(StreamGuard(endpoint_permit, model_permit, lease))
    }

    /// Extend (never shorten) the endpoint's cool-down by `delay`.
    pub(super) fn set_retry_after(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut guard = self.retry_after.lock().expect("retry_after poisoned");
        if guard.is_none_or(|cur| until > cur) {
            *guard = Some(until);
        }
    }

    /// Remaining cool-down, `None` when the endpoint is not parked.
    pub(super) fn retry_after_remaining(&self) -> Option<Duration> {
        self.retry_after
            .lock()
            .expect("retry_after poisoned")
            .and_then(|until| until.checked_duration_since(Instant::now()))
            .filter(|d| !d.is_zero())
    }

    /// Park until the cool-down has passed. `Err(())` — without sleeping —
    /// when it ends at or after `deadline`, the caller's own 429 budget: a
    /// cool-down set by a peer process or a different config must never hold
    /// this caller past it.
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
}

#[cfg(test)]
#[path = "limiter_tests.rs"]
mod tests;
