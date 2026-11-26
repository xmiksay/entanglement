//! The endpoint pool: one [`Limiter`] per normalized URL + API-key hash.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::limiter::{Limiter, RateLimits};
use super::{shared_state, HttpClient, RetryConfig};

/// Every endpoint's [`Limiter`], created on first use, so throttling on one
/// API never blocks another.
pub(super) struct EndpointPool {
    pub(super) endpoints: Mutex<HashMap<String, Arc<Limiter>>>,
    pub(super) config: RetryConfig,
    /// Fallback for an endpoint's unset limit axes.
    pub(super) default_limits: RateLimits,
    /// Where every endpoint in this pool keeps its cross-process state —
    /// fixed at client construction (#743).
    pub(super) shared: shared_state::SharedStateLocation,
}

/// RAII bump of a [`Limiter::waiters`] counter — display-only queue
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

impl HttpClient {
    /// The limiter for pool key `key`, created with `limits` on first use.
    pub(super) fn endpoint(&self, key: &str, limits: RateLimits) -> Arc<Limiter> {
        let mut map = self.pool.endpoints.lock().expect("endpoint pool poisoned");
        map.entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(Limiter::new(
                    limits,
                    self.pool.default_limits,
                    key,
                    &self.pool.shared,
                ))
            })
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
/// `https://api.z.ai/v4` collapse onto the same [`Limiter`] — and the
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
