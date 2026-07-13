# 0050. Per-endpoint connection pool, rate-limit, and retry

- Status: Accepted
- Date: 2026-07-13

## Context

Retry/backoff and rate-limiting lived on one shared `HttpClient`
(`entanglement-provider/src/client.rs`): a single `Arc<RateLimiter>` throttling at
50 RPM **across all providers**, plus per-call retry locals. With the YAML catalog
([ADR-0032](0032-yaml-provider-model-catalog.md), #118) a user configures many
endpoints (z.ai, OpenAI, Ollama, Anthropic, custom proxies) whose limits and
failure modes differ. One shared budget means a throttled endpoint starves every
other.

Two latent defects compounded this:

- **Retry was dead code (#193).** The client calls plain `.send()` (no
  `error_for_status`), so reqwest returns `Ok` for *any* HTTP status. A 429/5xx
  *response* is never a `reqwest::Error`, so `is_transient_error` never saw it and
  the retry loop never retried it — it was handled after the fact by the
  `!status().is_success()` branch in the backends. `extract_retry_after` (from a
  `reqwest::Error`) always returned `None`.
- **The RPM throttle never throttled.** `RateLimiter::acquire` took a semaphore
  permit into a `_permit` binding that dropped — releasing the token — at the end
  of the call, *and* spawned a timer that added another permit later. Net effect:
  capacity only ever grew.

- **`LlmSession` was a placeholder (#195).** Core's session handle newtyped
  `Box<dyn Llm>` and carried no meaningful connection state.

## Decision

Make the provider layer's resilience state a **per-endpoint pool** keyed by the
provider's base URL.

### `HttpClient` owns an `EndpointPool`

One tuned `reqwest::Client` is still shared — it already maintains a separate TCP
connection pool per host, so connection pooling needs no per-endpoint split. What
*is* split is the resilience state: an `EndpointPool` holds a
`Mutex<HashMap<String, Arc<EndpointState>>>`, lazily creating an `EndpointState`
on first use of each endpoint key. `EndpointState` carries:

- a **token-bucket `RateLimiter`** (capacity `rpm`, one token refilled every
  `60s / rpm`), and
- a **`Retry-After` window** (`Mutex<Option<Instant>>`): an instant before which
  no request to *that* endpoint may proceed.

`RetryConfig` (`max_attempts`, `initial_backoff`, `max_backoff`, `rpm`) is applied
per endpoint; defaults match the historical shared client (5 attempts, 200ms→30s,
50 RPM) — now *per endpoint*. `HttpClient::with_config` +
`RetryConfig::no_retry()` build variants.

### Retry classifies the response, per endpoint

`execute_with_retry(endpoint, request_fn)` now returns `reqwest::Response` and
classifies status **inside** the loop: a retryable status (429 or 5xx) with
attempts left is retried, honoring `Retry-After` (which also parks the whole
endpoint via its window) else exponential backoff + jitter; a permanent 4xx or an
exhausted retryable response is returned as `Ok` for the backend to surface as
today. Transport faults (`is_connect`/`is_timeout`/dropped stream) still retry.
This closes #193.

### `RateLimiter` actually consumes a token

`acquire` now `forget()`s the permit (so capacity drops) and schedules its return
after the refill interval — a real token bucket at the configured RPM.

### `LlmSession` (#195)

Core's handle stays a transport-free newtype around `Box<dyn Llm>`
([ADR-0006](0006-core-dependency-hygiene-gate.md) forbids reqwest in core), but
its boxed backend now references genuine per-endpoint state (RPM budget +
`Retry-After` window). The doc comment is updated to say so; the placeholder note
is resolved without leaking transport types into core.

## Consequences

### Positive

- Talking to different APIs is isolated: one throttled endpoint doesn't starve
  another. Adding a custom proxy via the catalog gets its own budget for free.
- Retry/backoff and `Retry-After` are no longer dead code (#193); the RPM throttle
  actually throttles.
- No new public plumbing in core; the seam is unchanged.

### Negative / neutral

- The endpoint key is the base URL string as passed, so two spellings of the same
  endpoint (trailing slash, host casing) would get separate budgets. Acceptable:
  a backend uses one stable base per provider.
- `EndpointState` is created lazily and never evicted. The set of endpoints is
  bounded by the catalog, so the map stays tiny; no eviction policy is warranted.

## Alternatives considered

- **Key by host/origin instead of base URL.** Rejected: z.ai's Coding Plan and
  pay-as-you-go tiers share a host but are distinct rate-limit domains; the base
  URL is the finest correct grain and is what a backend already holds.
- **A `reqwest::Client` per endpoint.** Rejected: reqwest already pools per host
  within one client, so this only duplicates connection state to no benefit.
- **Push retry/rate-limit config into the catalog `ProviderEntry`.** Deferred: the
  defaults are uniform today; per-provider RPM/`max_attempts` overrides can layer
  onto `RetryConfig` later without changing this structure.

## References

- Issue #217: provider connection pool — per-endpoint rate-limit, retry & backoff
- Issue #193: retry/rate-limit was effectively dead code (fixed here)
- Issue #195: `LlmSession` placeholder (per-endpoint state is what the handle
  references)
- Part of epic #190 (provider seam + per-endpoint pool)
- [ADR-0032](0032-yaml-provider-model-catalog.md): the YAML catalog that makes
  many endpoints configurable
- [ADR-0007](0007-streaming-llm-and-provider-crate.md): the provider crate that
  owns all LLM I/O and this resilience layer
