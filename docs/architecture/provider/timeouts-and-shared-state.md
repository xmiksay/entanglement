# LLM I/O §5b — Timeouts, throttle visibility & cross-process state

> Part of the [llm i/o module](../provider.md) of the [architecture overview](../../architecture.md).

**Timeouts — connect + await-headers + idle-gap, not whole-request** (#241,
#658): the shared `reqwest::Client` is built with `connect_timeout` only (30s
to establish TCP+TLS). A fixed whole-request `.timeout()` would abort a long
*healthy* LLM stream mid-turn (and its partials, already consumed, aren't
retryable) — and its 300s ceiling was also what capped `Stop` cancel latency
(#179). Between TCP+TLS establishment and the streamed body sits a third gap
`connect_timeout` and the idle-gap watchdog don't cover: a server that accepts
the connection and takes the request but never writes response headers. Before
#658 that hung `execute_with_retry` forever, pinning the endpoint's
concurrency permit and cross-process lease. `execute_with_retry` now wraps the
await-headers `request_fn()` call in `tokio::time::timeout(response_header_timeout,
…)` (`RetryConfig::response_header_timeout`, default 120s, aligned with
`STREAM_IDLE_TIMEOUT`); an elapsed timeout drops the silent connection and is
classified exactly like a transient transport fault, so it retries through the
same path up to `max_attempts` before surfacing `RetryError::HeaderTimeout`.
Once headers do arrive, liveness on the streamed body is enforced per chunk:
`client::spawn_byte_stream` forwards the SSE bytes over an mpsc channel under a
`tokio::time::timeout(STREAM_IDLE_TIMEOUT, …)` watchdog (120s idle gap), so a
slow-but-alive stream runs to completion while a hung one dies fast. Both
`OpenAiLlm`, `AnthropicLlm`, and `GeminiLlm` use this one helper. **The pump
also races every read against the consumer dropping its
receiver** (`tokio::select!` against `tx.closed()`, #552): before this, the
`StreamGuard` it holds (the endpoint/model permits and the cross-process lease)
released only on the pump's *next* chunk-send failure or the idle-gap timeout
— neither of which fires while the body is silently paused, so a `Stop` mid
reasoning pause, or the OpenAI-compat parser's `[DONE]`-triggered `break 'outer`
while a keep-alive proxy holds the connection open, both left the consumer's
receiver dropped with the pump none the wiser, holding the guard for up to the
full 120s. Racing the read against `tx.closed()` frees it the moment the
consumer is gone. **Retry** classifies the *response* status inside the loop — a 429/5xx response
(not just a `reqwest::Error`) is retried, not silently dropped; before #217 those
responses came back as `reqwest::Ok` and were never retried (#193). A 5xx retries
with exponential backoff + jitter up to `max_attempts`; a 429 retries until clear
(ADR-0111, [surface & resilience](surface-and-resilience.md)). Transport `reqwest::Error`s are classified by
`is_transient_error`: connect/timeout faults, retryable statuses, **and
request-send faults** (`is_request()` — a dropped keep-alive connection reset
*between* requests, which reqwest renders as `"error sending request for url …"`
and is *not* `is_connect()`; safe to resend since no response body was consumed)
all retry; anything else is permanent.

**Throttle visibility.** `HttpClient::throttle_status() -> Option<ThrottleStatus>`
is a read-only snapshot over the live pool (in-flight/cap, `Retry-After` remaining,
whether the pacing gate is penalized) for the most-throttled endpoint, or `None`
when every endpoint is at rest. Since #521 the reported in-flight/cap is
whichever is **binding** — the endpoint's own, or a per-model slot whose
occupancy ratio is currently tighter (`model: Option<String>` names it when
so); an idle or never-created model slot never shadows a genuine
endpoint-wide cool-down. It feeds no request logic — the TUI polls it each
frame to show a throttle indicator only while backing off (see heads doc),
rendering the model id alongside the host when it is the binding constraint.
`ThrottleStatus` also carries `next_request_in: Option<Duration>` — the AIMD
pacing gate's own `next_slot` countdown, surfaced only while `penalized`
(#517, [ADR-0141](../../adr/0141-wire-visible-throttle-transitions.md)) — so the
TUI's "pacing" label can show a live wait, not just the bare word.
**Cross-process facts are folded in too (#552):** before this, `ThrottleStatus`
read only in-process state, so a peer process's parked 429, a lease it held
via the shared gate, or a caller of *this* process blocked inside
`SharedGate::acquire` all read as "at rest" — the incidents that most needed
surfacing showed as nothing wrong. `backoff_remaining` now takes the max of
this process's own cool-down and whatever `SharedGate::peek` reads back from
the shared state file (a lock-free `fs::read` — the file is always replaced
via an atomic rename, so a concurrent writer is never observed mid-write,
only up to one write cycle stale, which is fine for a status label though not
for `acquire`'s admission decision). `shared_leases: Option<usize>` reports
the live cross-process lease count (`None` when sharing is disabled or the
file is unreadable) and factors into both `is_throttled()` and the runtime
throttle responder's `classify()`, so a sibling saturating the shared cap
reads as busy even while this process's own semaphore has room. `waiters:
usize` (#517) is a display-only counter — bumped just
before, and dropped via a `WaiterGuard` right after, this endpoint's own
`concurrency.acquire_owned().await` — reporting how many callers are queued
behind the permit, not yet admitted. The TUI's `throttle_label` gains a
`(shared X/cap)` suffix when the shared lease count disagrees with the local
`in_flight`, and a `· Nq` suffix while callers are queued.
`HttpClient::throttle_statuses() -> Vec<ThrottleStatus>` is the sibling that
snapshots **every** resolved endpoint (not just the most-throttled): the
runtime's `throttle::spawn_throttle_responder` polls it every 500ms and emits
a wire-visible `OutEvent::Throttle` on each endpoint's own enter/exit
transition, so a stdio/WS head sees the same stall the TUI renders directly
(ADR-0141 — engine-global, not per-session, matching this pool's own
per-endpoint model). `RetryConfig` (`max_attempts`, `initial_backoff`,
`max_backoff`, `rpm`) tunes the *failure* path; `HttpClient::with_config` +
`RetryConfig::no_retry()` build variants (tests use the latter).
`execute_with_retry` also takes a **per-call** `retry: Option<RetryConfig>`
override (#660) — `None` uses the pool's own config unchanged (every
primary-turn request, every MCP request); a caller with a legitimately
different patience budget (the aux narrate/session-title paths always, summarize only on a
pinned `summarize` model — `LlmRequest::retry = Some(RetryConfig::minimal())`) can swap
just the failure-path knobs while still riding the same endpoint's
RPM/concurrency/429 admission as everyone else. The MCP startup handshake
that first used it ([ADR-0169](../../adr/0169-startup-mcp-connect-is-concurrent-and-fast-fail.md))
is retired with the startup connect (ADR-0226; its `handshake_retry`
parameter removed, #855). This
per-endpoint state is the reason a session carries **no** per-session connection
handle: the `LlmSession` newtype was collapsed to a plain `Box<dyn Llm>` (#195,
[ADR-0062](../../adr/0062-collapse-llmsession-placeholder-newtype.md)) — resilience
belongs to the endpoint, shared across sessions, not to the conversation. A
**live model/provider switch** (#218,
[ADR-0063](../../adr/0063-realtime-model-provider-switch.md)) rebuilds that
`Box<dyn Llm>` from a `ResolvedModel` the runtime resolves against this catalog +
the warm per-endpoint client, so switching mid-session neither restarts the engine
nor cold-starts the pool.

**Shared across processes, not just across sessions** (#523,
[ADR-0144](../../adr/0144-file-backed-shared-endpoint-state-across-instances.md)):
everything in [provider](../provider.md) up to here coordinates every session *within one `skutter` process* —
but two processes talking to the same `(endpoint, api-key)` used to each run a
fully independent `EndpointPool`, so N processes collectively sent up to N×
the configured RPM and held N× the concurrency cap against a provider with no
idea more than one client existed. `EndpointState` gains a
`client::shared_state::SharedGate` — a file-backed cross-process ledger at
`${data_dir}/entanglement/endpoints/<sha256(pool_key)>.state`, guarded by an
advisory `fd-lock` read-modify-write exactly like the managed config files
(#329, ADR-0084; independently re-implemented in `entanglement-provider`
rather than depended on, since the provider crate is the leaf and takes no
`entanglement-*` dependency, ADR-0053). The on-disk shape and locked
read-modify-write mechanics live in a sibling `client::shared_store` module
(split out from `shared_state` purely to keep both under the 400-line file
cap, #552); `shared_state` owns `SharedGate`/`SharedLease` and the admission
policy. Shared: the RPM token-bucket ledger, a
lease-based in-flight concurrency count (each admitted request holds a lease —
id, owning pid, expiry — renewed on a heartbeat and pruned by TTL if its
process dies without releasing it, so a crash recovers the slot rather than
leaking it permanently), and the `Retry-After` cool-down deadline (one
process's 429 parks every sibling's next `acquire`). **Not** shared in v1: the
AIMD pacing gate stays per-process — once the budget itself is bounded
correctly in aggregate, per-process pacing converges on the same signal (see
the ADR for the full reasoning). `execute_with_retry`'s loop calls
`endpoint.shared.acquire(rpm, concurrency, rl_deadline)` **last** — after
`wait_for_retry_after`/`limiter.acquire` *and* after both in-process permits
(model, then endpoint) — not before them (#546, fixing a starvation bug:
acquiring the shared lease first let a caller queued on its own model's
semaphore sit holding this scarcer, process-wide resource for as long as it
waited, starving sibling `skutter` processes even though the provider had
room; it also meant the shared RPM ledger, stamped on admission, was stamped
at the start of that wait rather than at the actual send). `acquire` is
itself bounded by `rl_deadline` (#547) — waiting past it returns `Err(())`
rather than polling forever on a persisted cool-down or a saturated cap. The
returned lease is held in `StreamGuard` alongside the endpoint/model permits
and all three release together; a 429 also `.await`s
`endpoint.shared.mark_retry_after(delay)` so the cool-down reaches siblings,
not just this process — awaited rather than fired off as a detached
`tokio::spawn` (#547), since a detached task can be dropped mid-flight by a
short-lived one-shot `run` exiting right after. Falls back silently to pure
in-process gating (today's pre-#523 behavior) when the state directory is
unwritable or disabled via `ENTANGLEMENT_DEBUG=no-shared-state` — an
operator who wants genuinely separate per-instance budgets already gets that
for free by giving each instance its own key/base URL, since the pool key
itself isolates them.

**The location is injected per client, resolved from env only by default**
(#743): an `HttpClient` carries a `SharedStateLocation` (`Disabled` |
`Dir(path)`) fixed at construction and handed to every `EndpointState` it
creates. `HttpClient::new`/`with_config` resolve it once via
`SharedStateLocation::from_env()` — `ENTANGLEMENT_DEBUG=no-shared-state` >
`ENTANGLEMENT_SHARED_STATE_DIR` (an undocumented subprocess-test hook since
ADR-0213) > `${data_dir}/entanglement/endpoints` > `Disabled` — so production behavior is unchanged; `HttpClient::with_shared_state(location,
config)` pins it explicitly (a per-test temp dir, `Disabled`, or an embedder
running several isolated pools in one process). Tests never mutate the
process env to steer it: the precedence is unit-tested over an injected env
lookup, and in the provider crate's own `cfg(test)` build the data-dir
fallback panics, so a test that forgets to inject a location fails instead
of touching the developer's live endpoint state. `prune_stale` is the
env-resolved convenience over `SharedStateLocation::prune_stale`.

**Lease release is synchronous, and the TTL backstop is tighter** (#547):
`SharedLease::drop` used to only cancel its background renewal task, which
then asynchronously removed the lease from the shared file — a detached
`tokio::spawn` a short-lived one-shot `run` (or a SIGINT/SIGTERM shutdown)
could tear the tokio runtime down before it ever got scheduled, leaving a
live-looking lease for the next launch to block on. `Drop` now removes the
lease **synchronously**, in-line, covering every clean exit path (normal
completion, a `.abort()`-driven task teardown, SIGINT/SIGTERM). `LEASE_TTL`
is kept close to `LEASE_RENEW_INTERVAL` (~2×: 120s vs. 60s, was 180s) rather
than generously above it — the TTL is now purely the backstop for the one
case synchronous release can't cover: a `SIGKILL`, where nothing runs at all.

**Orphaned shared-state files are swept, not left to accumulate** (#551):
nothing evicts a `.state`/`.lock` pair when its pool key stops being used — a
`/key` rotation, a catalog `base_url` edit, a decommissioned provider — so
without an explicit sweep they pile up under the state directory forever.
`client::prune_stale(max_idle)` walks the directory and removes any pair that
is both **idle** (`.state` mtime older than `max_idle`) and **empty** (no live
lease, no pending cool-down, no request in the trailing RPM window) — an
endpoint still in real use always fails the "empty" check regardless of file
age, so a live budget can never be swept out from under it. The runtime calls
this twice: a best-effort startup sweep (`max_idle = 1h`, `main.rs`, mirroring
`SessionStore::prune`'s role for session logs) and a short-`max_idle` (5s)
sweep fired from the TUI's `/key` submit handler
(`entanglement-runtime::tui::app::key`, `tokio::task::spawn_blocking` — the
sweep locks and `fsync`s files, kept off the keypress path) so a rotated-away
key's file doesn't wait a full hour once every session bound to it has moved
off. This does **not** force an already-bound session to rebind — that stays
an explicit `/model` switch (ADR-0063); it only reclaims the file once nothing
references it anymore.

**Request-body logging is opt-in and symmetric** (#165): every client emits a
`debug!` *summary* per request (model, message/tool counts — no payload). The
full request body — system prompt, the **entire conversation**, tool schemas
(repo/user data; API keys never appear, they ride in headers) — is logged only
through the shared `client::log_request_body(provider, &body)` helper, gated
behind `ENTANGLEMENT_DEBUG=bodies` and truncated to 8 KiB on a UTF-8 boundary.
Raising `RUST_LOG` verbosity alone will **not** emit it; the flag is a separate,
explicit opt-in. Both `OpenAiLlm` and `AnthropicLlm` route through the one helper
so body logging is identical across backends.

---

Previous: [§5b — Advertised surface, web search & per-endpoint resilience](surface-and-resilience.md) · Next: [§5b — Provider/model catalog & advanced wire facts](catalog.md)
