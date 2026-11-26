//! The one rate/concurrency limiter (#918).
//!
//! Every LLM request admits through one [`Limiter`] per endpoint (pool key =
//! normalized base URL + API-key hash). It enforces a [`RateLimits`] — the
//! single config shape, read from a catalog entry's `limits:` — on two keys:
//! the endpoint itself, and optionally the requested model on that endpoint
//! (z.ai caps concurrency per model). Each key is one [`Gate`]
//! (`gate.rs`): a sliding 60 s window over requests (`rpm`) and estimated
//! tokens (`tpm`), plus a semaphore over in-flight requests (`concurrency`). The endpoint's
//! `Retry-After` cool-down lives here too, so the retry loop and the
//! admission path read one state.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::time::sleep;

use super::endpoint::WaiterGuard;
use super::gate::Gate;
use super::{shared_state, StreamGuard};

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

/// One endpoint's limiter: its own gate, a gate per capped model, the
/// `Retry-After` cool-down, and the cross-process lease file.
pub(super) struct Limiter {
    /// The client's fallback for unset endpoint axes.
    defaults: RateLimits,
    endpoint: Arc<Gate>,
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
            endpoint: Arc::new(Gate::new(limits.or(defaults))),
            models: Mutex::new(HashMap::new()),
            waiters: AtomicUsize::new(0),
            retry_after: Mutex::new(None),
            shared: shared_state::SharedGate::new(pool_key, shared),
        }
    }

    /// The endpoint gate, set to `limits`. Each request admits under its own
    /// caller's limits — an admin's catalog edit applies at once, and two
    /// callers sharing a key with different limits each get theirs — while
    /// the window and the in-flight count stay the key's one shared state.
    pub(super) fn endpoint_gate(&self, limits: RateLimits) -> Arc<Gate> {
        let limits = limits.or(self.defaults);
        if self.endpoint.update(limits) {
            tracing::debug!(?limits, "endpoint limits changed");
        }
        self.endpoint.clone()
    }

    /// The endpoint gate, for status display.
    pub(super) fn current_endpoint_gate(&self) -> Arc<Gate> {
        self.endpoint.clone()
    }

    /// The gate for `model` on this endpoint, `None` when the model carries
    /// no limits of its own. Set to `limits` like
    /// [`endpoint_gate`][Self::endpoint_gate].
    pub(super) fn model_gate(&self, model: &str, limits: RateLimits) -> Option<Arc<Gate>> {
        if limits == RateLimits::default() {
            return None;
        }
        let mut models = self.models.lock().expect("model gates poisoned");
        let gate = models
            .entry(model.to_string())
            .or_insert_with(|| Arc::new(Gate::new(limits)));
        if gate.update(limits) {
            tracing::debug!(model, ?limits, "model limits changed");
        }
        Some(gate.clone())
    }

    /// Admit one request: model permit and window, then endpoint permit and
    /// window, then the cross-process lease. Narrowest first, so a caller
    /// blocked on its own model never holds the endpoint slot (or a peer
    /// process's lease) hostage. `Err(())`: the cross-process gate would
    /// need to wait past `deadline`; the request is then taken back out of
    /// both windows, since it was never sent.
    pub(super) async fn admit(
        &self,
        model: &str,
        limits: RequestLimits,
        tokens: u32,
        deadline: Instant,
    ) -> Result<StreamGuard, ()> {
        let endpoint = self.endpoint_gate(limits.endpoint);
        let model_gate = self.model_gate(model, limits.model);
        let (model_permit, model_sent) = match &model_gate {
            Some(gate) => {
                let permit = gate.acquire().await;
                (Some(permit), gate.pace(tokens).await)
            }
            None => (None, None),
        };
        let waiter = WaiterGuard::new(&self.waiters);
        let endpoint_permit = endpoint.acquire().await;
        drop(waiter);
        let endpoint_sent = endpoint.pace(tokens).await;
        let lease = self
            .shared
            .acquire(
                endpoint.limits().rpm.unwrap_or(u32::MAX),
                endpoint.cap().unwrap_or(usize::MAX),
                deadline,
            )
            .await;
        let Ok(lease) = lease else {
            if let (Some(gate), Some(at)) = (&model_gate, model_sent) {
                gate.unrecord(at, tokens);
            }
            if let Some(at) = endpoint_sent {
                endpoint.unrecord(at, tokens);
            }
            return Err(());
        };
        Ok(StreamGuard(Some(endpoint_permit), model_permit, lease))
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
