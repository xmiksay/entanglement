# 0144. File-backed shared endpoint state across instances

- Status: Accepted
- Date: 2026-07-31

## Context

The per-endpoint resilience pool (`EndpointState`, ADR-0050/ADR-0111/ADR-0122)
lives in one process's memory (`EndpointPool { endpoints: Mutex<HashMap<..>> }`,
`entanglement-provider/src/client/mod.rs`). It is keyed by `(base URL, API-key
hash)`, so every session/sub-agent *within one `skutter` process* correctly
shares one RPM budget, one concurrency cap, and one `Retry-After` cool-down.

Nothing about that key is process-scoped, but the state itself is. Running two
`skutter` processes against the same provider key — one per project, or a TUI
session beside a `serve` head — gives each its own `EndpointState` for the
identical endpoint. Each believes it owns the full budget (`rpm: 60` means "60
to me", not "60 total"), so N processes collectively send up to N× the
configured RPM and hold N× the concurrency cap against a provider that has no
idea it's talking to more than one client.

Today this is only softened indirectly: each process's own AIMD pacing
(ADR-0111) penalizes only *after it personally* sees a 429, and the
park-and-retry loop absorbs collisions one at a time. The budget is discovered
by failing, per process, instead of known in advance and shared. Two
instances actively fight rather than coordinate: one instance's cool-down
does not stop the other from immediately re-saturating the endpoint the
instant its own request lands.

Precedent for cross-instance coordination already exists in this codebase:
the managed config files (`grants.yml`, `agent-models.yml`, the managed
`.env`) are advisory-locked with `fd-lock` across concurrent `skutter`
instances (#329, ADR-0084, `entanglement-runtime::config::lock::with_locked_file`)
so a concurrent read-modify-write cycle doesn't clobber another instance's
write.

## Decision

Add a **file-backed shared token bucket** (the issue's candidate design 1) as
a second, cross-process gate layered *in front of* the existing in-process
`EndpointState`, not a replacement for it.

### What's shared, what stays per-process

Shared, in a small state file per pool key:

- **RPM budget** — a token-bucket ledger of request timestamps in the
  trailing 60s window, pruned on every access.
- **Concurrency** — a lease-based in-flight count. Each admitted request
  holds a lease (owning pid + expiry) for as long as its streamed response
  body is open, renewed on a heartbeat; the combined lease count across every
  process sharing the file is what's checked against the cap, not any one
  process's own count.
- **`Retry-After` cool-down** — a shared deadline. One process's 429 sets it;
  every process (including the one that saw the 429) is parked until it
  passes.

Deliberately **not** shared in v1:

- **The AIMD pacing gate** (`RateLimiter`, ADR-0111) stays per-process. Once
  the RPM budget and the concurrency cap are themselves shared and correctly
  bounded, per-process pacing is smoothing a signal that is already correct
  in aggregate — a second cross-process pacing ledger would add file I/O and
  complexity for a problem the shared budget already solves. If practice
  shows the per-process pacing still causes thundering-herd behavior right
  at the moment a shared slot frees up, that's a well-scoped future addition
  (see Alternatives).

### File layout and locking

One JSON state file per pool key (the same `pool_key(endpoint, api_key)` —
base URL plus a hash of the API key — the in-process pool already computes),
SHA-256-hashed to a filename so it never embeds a raw key or an arbitrary URL
as a path component:

```
${data_dir}/entanglement/endpoints/<sha256(pool_key)>.state
```

Access is a read-modify-write cycle under an advisory `fd-lock` on a sibling
`.lock` file — the exact pattern `config::lock::with_locked_file` established
for the managed config files (#329, ADR-0084), independently re-implemented in
`entanglement-provider` (not called by reference: the crate takes **no**
`entanglement-runtime` dependency, ADR-0053 — provider is the leaf, runtime
depends on it, never the reverse). Held only for the duration of a single
read-parse-mutate-serialize-write step (microseconds), never across the
network request itself — a request that has to wait polls: attempt admission
under the lock, and if the budget/cap/cool-down says no, sleep a short
interval and retry. There is no blocking wait *inside* the lock; two
processes never hold each other up for longer than one file read+write.

### Lease-based concurrency, not a shared semaphore

A cross-process semaphore has no clean release-on-crash story. A **lease**
does: each admitted request writes `{ id, pid, expires_at }` into the state
file and refreshes `expires_at` on a periodic heartbeat while its request is
in flight (well inside the lease TTL, so a live holder never lapses under
ordinary scheduling jitter). On completion — success, error, or the process
being killed outright — the lease is either explicitly removed (the clean
path) or simply stops being renewed and is pruned by the next process to
touch the file once its TTL elapses. A crashed instance's slots are always
recovered within one TTL window; nothing is leaked permanently, and no
process ever needs to detect *that a specific peer died* — the shared state
just forgets stale entries on its own schedule.

### Fails closed to "don't share," not to "don't work"

If the state directory can't be created or written (permissions, a read-only
filesystem, a sandboxed test environment), the gate degrades to `None` and the
caller falls back to pure in-process behavior — today's pre-#523 posture.
Instances simply stop coordinating; they do not error or refuse to make
requests. The same fallback covers the explicit opt-out below.

### Opt-out

An operator who wants **separate** per-instance budgets on purpose — e.g.
running two unrelated projects against literally the same key and wanting
neither to throttle the other — already gets that for free by giving each
instance its own key/base URL (the existing `(base URL, API-key hash)` pool
key already isolates them, unchanged by this ADR). For the same-key case, a
process-wide `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1` disables the shared
gate outright and reverts to pre-#523 in-process-only behavior.

## Consequences

- **Positive**: two (or more) `skutter` processes sharing a provider key now
  collectively respect the configured RPM/concurrency budget instead of each
  applying it in full; a 429 seen by one instance parks the others instead of
  them immediately re-triggering it.
- **Positive**: no daemon, no new long-lived process, no socket protocol —
  the only new dependency is `fd-lock`, already vendored for #329.
- **Positive**: crash-safe by construction (lease TTL), with no explicit
  liveness detection needed.
- **Negative**: adds a filesystem read-modify-write to the request path.
  Bounded by the RPM ceilings actually in play (tens of requests per minute,
  not per second) and by the lock only ever being held for a single
  synchronous read+write — negligible next to an LLM request's own
  network/generation latency, and skipped entirely for a request that had to
  poll-wait, which pays the same cost it would have paid anyway.
- **Negative**: the shared ledger is coarser than a live cross-process
  semaphore would be — admission is polled, not signaled, so a slot freeing
  up is noticed only on the poll cadence, not instantly. Acceptable: the
  in-process gates (pacing, the local concurrency permit) already tolerate
  this kind of slack, and the alternative (a broker) is a different class of
  system.
- **Neutral**: `EndpointState` gains a field holding the process's handle to
  this file (`shared_state::SharedGate`); `StreamGuard` gains a third,
  optional member (the held lease) alongside the existing endpoint/model
  semaphore permits, released together when the streamed body ends.

## Alternatives considered

1. **Local broker daemon** (first instance owns the pool, others acquire
   permits over a unix socket). Rejected for v1: correct and eventually
   precise, but a category shift — it needs lifecycle management (who starts
   it, who owns it, what happens when the owning instance exits while others
   are still attached), a socket wire protocol, and becomes a single point of
   failure the current design has no equivalent of. Revisit if file-polling
   latency proves to matter in practice.
2. **Static partitioning** (`{NAME}_RPM` divided by a configured instance
   count). Rejected outright: wastes budget whenever any instance is idle,
   requires the operator to know and configure the instance count up front,
   and doesn't hold up the moment a third instance shows up unannounced.
3. **Share the AIMD pacing state too** (not just RPM/concurrency/cool-down).
   Deferred, not rejected: once the budget itself is shared and correctly
   bounded, per-process pacing converges on the same signal in practice (see
   Decision). Would add a fourth field to the shared ledger and file I/O on
   every single request's pacing decision, not just on admission/renewal —
   revisit only if evidence shows the per-process gate alone still causes
   herd behavior right as a shared slot frees.
