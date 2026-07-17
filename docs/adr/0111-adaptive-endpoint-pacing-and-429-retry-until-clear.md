# 0111. Per-endpoint concurrency cap, adaptive pacing, and bounded 429 retry

- Status: Accepted
- Date: 2026-07-17

## Context

[ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) made the
provider layer's resilience state a per-endpoint pool keyed by `(endpoint,
api-key)`: a token-bucket `RateLimiter`, a shared `Retry-After` cool-down window,
and a retry loop bounded by `RetryConfig::max_attempts` (5). The pool is shared
across every session/backend clone that talks to the same endpoint (one
`HttpClient` built once, cloned into every factory and the model resolver — the
`Arc<EndpointPool>` is shared).

In practice, **spawning many sub-agents against one provider (z.ai) hung the whole
session**: the main agent parked waiting on its `spawn` tool while the spawned
sub-agents were stuck — "one session runs, the rest 429", or worse, no result at
all. Several defects compounded:

1. **No concurrency bound.** Nothing capped how many requests were *in-flight at
   once*. N spawned sub-agents opened N SSE streams against the provider
   simultaneously. An LLM stream (especially a **thinking/reasoning** model) stays
   open for the whole generation — seconds to minutes — and a provider's real
   ceiling is *concurrency*, not just RPM. So the fan-out itself overran the limit.
2. **Burst overshoot.** The token bucket started *full* (`Semaphore::new(rpm)` =
   50 permits), so even the RPM throttle let 50 callers fire at once.
3. **A headerless 429 never parked the shared window.** `set_retry_after` was
   called only when the server sent a `Retry-After` header; z.ai's 429s often omit
   it, so each failing session backed off independently and re-collided.
4. **Exhausted retries surfaced a hard error** *or*, once retries were made
   "retry-until-clear" for a session's sake, **hung forever** — a stuck sub-agent
   never returned a `ToolResult`, so the parent's parked turn waited indefinitely.

The framing that resolved the tension: rate-limiting is a property of the
**endpoint pool**, not of a session; the fix must be concurrency-aware (so
requests actually *complete* a few at a time) and must never let a stuck endpoint
hang the parent — a saturated endpoint should *fail* a sub-agent's turn, not stall
it.

## Decision

Keep the per-endpoint pool keyed by `(endpoint, api-key)` (ADR-0050 unchanged) but
add the missing control and fix the loop's behavior at the shared coordination
point — `EndpointState` (`entanglement-provider/src/client.rs`).

### Primary fix: a per-endpoint concurrency cap held across the stream

`EndpointState` gains `concurrency: Arc<Semaphore>` (default **3** permits,
`RetryConfig::concurrency`, overridable via `ENTANGLEMENT_MAX_CONCURRENCY`).
`execute_with_retry` acquires an owned permit just before sending and returns it
to the backend as an opaque **`StreamGuard`** that `spawn_byte_stream` moves into
the body-pump task — so the permit is held for the whole request **and its
streamed body** and released only when the stream ends. The cap therefore counts
*open streams* (thinking generations included), which is what a provider actually
limits. Many spawned sub-agents queue on `acquire()` and run a few at a time; the
4th waits for a slot rather than 429-storming. This is backpressure, not a hang —
a queued caller always proceeds as slots free, and the wait is not charged to the
429 budget below. The permit is released (dropped) during any retry backoff so a
waiting caller can use the slot meanwhile.

### Adaptive pacing gate (RPM smoothing) replaces the bursty token bucket

The `Semaphore` token bucket (defect #2) is replaced by a **spacing gate**: per
endpoint, `acquire` reserves the next slot `interval` after the last, so callers
get distinct, spaced slots (base `interval = 60s / rpm`). It is **adaptive
(AIMD)** — `penalize` doubles `interval` on each 429 (capped at
`base × SLOWDOWN_CAP`, cap = 8), `relax` steps it back toward base on each success
— so a too-high default RPM self-corrects and recovers. A single healthy session
never notices (its next request arrives well after `next_slot`).

### Every 429 parks the shared window; 429 retries are bounded, then surface

In `execute_with_retry`, a 429 now calls `penalize()` **and**
`set_retry_after(delay)` **unconditionally** (not only with a header, fixing defect
#3), so all concurrent callers back off together. It is retried on a patient
schedule — `rate_limit_initial_backoff` (5s) ramping to `rate_limit_max_backoff`
(≈10 min), a server `Retry-After` overriding the wait — **until it clears or the
overall `rate_limit_max_elapsed` budget (≈15 min) is exceeded**, at which point the
429 is returned as `Ok` for the backend to surface as an error (fixing defect #4:
a saturated endpoint fails the turn instead of hanging the parent). A 429 does not
consume `max_attempts`. Genuine failures (retryable 5xx, transport faults) stay
bounded by `max_attempts` as before.

## Consequences

### Positive

- The real cause of "spawn N sub-agents → hang" is fixed: at most `concurrency`
  streams run at once, the rest queue and complete, so the parent's `spawn`
  resolves.
- The pool genuinely coordinates across sessions: a 429 slows and parks *every*
  caller of that endpoint.
- The default RPM/concurrency no longer has to be exactly right: AIMD pacing
  self-tunes RPM, and a saturated endpoint degrades to queueing, not storming.
- A stuck endpoint fails a sub-agent's turn within a bounded budget rather than
  hanging its parent forever.

### Negative / neutral

- Holding a permit across a slow **thinking** SSE means a queued sub-agent can wait
  minutes for a slot behind long reasoning generations. This is intended
  backpressure; raise `ENTANGLEMENT_MAX_CONCURRENCY` if a workload wants more
  parallel thinking streams. (The 120s idle-gap watchdog still bounds a genuinely
  hung stream per-chunk, unaffected.)
- Concurrency is one global default (3) applied per endpoint, not yet per-provider
  catalog data — a deferred follow-up (see the deferred-work ledger).
- The pacing gate replaces ADR-0050's token bucket, so its `Semaphore`-shaped unit
  tests were rewritten around `base`/`interval`/pacing; `RetryConfig::rpm` now
  seeds base spacing rather than a permit count.

## Alternatives considered

- **Concurrency permit released at response headers (not across the stream).**
  Rejected: an open thinking stream still counts against the provider's
  concurrency limit, so releasing early re-admits the storm.
- **RPM/pacing only, no concurrency cap.** Rejected: time-spacing can't bound
  *concurrent open streams* when each stream outlives the spacing interval — the
  binding constraint for spawned sub-agents.
- **Retry 429 forever.** Rejected: a sub-agent that never clears hangs its parent;
  a bounded budget that surfaces an error lets the parent continue/report.
- **Per-provider concurrency in the catalog now.** Deferred: a global default +
  env override covers the reported case; per-provider tuning can layer on later.

## References

- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): the
  per-endpoint pool this amends (keying, `Retry-After` window, `RetryConfig`).
- [ADR-0007](0007-streaming-llm-and-provider-crate.md): the provider crate that
  owns all LLM I/O and this resilience layer.
- Issue #193 / #217: retry-was-dead-code and the original per-endpoint pool.
