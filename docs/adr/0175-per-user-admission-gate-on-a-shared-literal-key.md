# 0175. Per-user admission gate on a shared literal API key

- Status: Accepted
- Date: 2026-08-06
- Related: [ADR-0147](0147-multi-user-mode-embedder-api.md) (multi-user
  embedder API — "Consequences" scoped this out for v1),
  [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) (the
  per-endpoint pool key this layers on top of), [ADR-0140](0140-per-model-concurrency-cap-layered-on-endpoint-cap.md)
  (the admission-layer precedent this composes with)

## Context

[#522](https://github.com/xmiksay/entanglement/issues/522)/ADR-0147 gave a
multi-user embedder per-user rate-limit isolation "for free": ADR-0050's pool
keys an `EndpointState` by `(base_url, sha256(api_key))`, so two users
configured with *distinct* keys on the same provider already land in separate
states with independent RPM/concurrency/429 cool-down.

Two users configured to *share* one **literal** key don't get that for free —
they land in the exact same `EndpointState`, and `HttpClient::endpoint`'s
"first caller wins" sizing means whichever user's session happens to resolve
first sets the endpoint's aggregate rpm/concurrency for both. ADR-0147 called
this out explicitly and scoped it out of v1 (its "Consequences" section,
[deferred-work-ledger row 12](../deferred-work-ledger.md),
[#632](https://github.com/xmiksay/entanglement/issues/632)), recommending
either a user dimension in the pool key, or a per-user admission gate layered
above it.

## Decision

**A per-user admission gate, layered above the shared endpoint pool** —
option (b), not a pool-key change. This mirrors ADR-0140's per-model gate
almost exactly, just keyed by `UserId` instead of model id, and composes with
the mechanism ADR-0140 laid down rather than duplicating it.

### Why not widen the pool key (option a)

Adding a user discriminator to `pool_key` would make two users on the *same*
literal key get fully independent `EndpointState`s — including independent
429 cool-downs and independent cross-process shared-state files
(ADR-0144/ADR-0156). That's wrong: a 429 from the provider is a fact about
the *key*, not about which of its users happened to trigger it — the whole
point of ADR-0111's endpoint-wide cool-down is that every caller sharing a key
backs off together. Splitting the pool key per user would silently break
that: user A's request 429s, but user B's requests keep firing at full pace
against a key that's already being throttled, immediately re-triggering the
429 A's back-off was supposed to prevent. The gate approach keeps exactly one
`EndpointState` (and one cool-down, one cross-process file) per real
`(endpoint, key)`, with the per-user layer narrowing only *admission*, never
the shared failure response.

### `HttpClient::with_user_budget`, not a new `execute_with_retry` parameter

Every wire client (`OpenAiLlm`, `AnthropicLlm`, `GeminiLlm`) already holds an
`HttpClient` clone and calls `self.http.execute_with_retry(...)`. Rather than
adding a `user`/`user_rpm`/`user_concurrency` parameter to that call (which
would touch all three wire client structs, their factories, and every
existing call site in `main.rs`), `HttpClient` itself gains an optional
`user_budget: Option<Arc<UserBudget>>` field and a
`with_user_budget(budget) -> Self` builder that returns a clone carrying it.
The pool (`Arc<EndpointPool>`) is still shared — only the budget attached to
*this* handle differs. `entanglement-runtime::multi_user::provider::
resolve_for_user` calls `with_user_budget` once, before constructing that
user's `Llm` factory, using **that user's own** catalog `rpm`/`concurrency`
(the same `ProviderEntry` fields already flowing into the endpoint-sizing
call — now also captured as this user's private slice). Every single-user
caller (`main.rs`) never calls `with_user_budget`, so `user_budget` stays
`None` and admission is byte-identical to before this ADR.

### `UserSlot`, mirroring `ModelSlot`

`EndpointState` gains `user_budgets: Mutex<HashMap<UserId, Arc<UserSlot>>>`,
lazily populated as `model_concurrency` already is. `UserSlot` bundles an
optional concurrency `Semaphore` (sized from the user's own `concurrency`) and
an optional pacing `RateLimiter` (sized from the user's own `rpm`) — unlike
the endpoint's own limiter, the per-user one is **not AIMD-adaptive**: a 429
is a fact about the whole endpoint, not one user's slice of it, so
`penalize`/`relax` stay endpoint-wide only; the user gate just paces at a
fixed rate. A user with neither configured carries no gate at all — admits
solely through the model/endpoint gates, unchanged from before this existed.
`EndpointState::user_slot` self-corrects a changed budget instead of latching
the first value seen, exactly like `model_slot` (#550) — a stale/wrong value
reaching it first (a catalog reload, a race at startup) must not stick for
the rest of the process.

### Acquisition order: user, then model, then endpoint, then the shared lease

ADR-0140 established model-before-endpoint so a caller blocked on its own
model's slot never holds the scarcer endpoint permit hostage. The user gate
extends the same reasoning one level narrower: acquired **first**, before the
model permit, so a caller blocked on its own user slot never holds a resource
shared with other users *or* models hostage while it waits. The full order —
user, model, endpoint, cross-process shared lease — is applied uniformly on
every call path, so it stays deadlock-free exactly as ADR-0140's two-level
order was. `StreamGuard` widens to hold all four (three owned permits plus the
optional shared lease).

## Consequences

### Positive

- Two users sharing one literal key each stay within their own configured
  rpm/concurrency slice, regardless of which one's session happened to size
  the shared endpoint first — the gap ADR-0147 explicitly left open closes.
- Zero change to `pool_key`, `EndpointState`'s cool-down/pacing, or the
  cross-process shared-state file format (ADR-0144/ADR-0156) — the endpoint
  stays one real resource with one failure response, exactly as before.
- Zero change to `OpenAiLlm`/`AnthropicLlm`/`GeminiLlm`, their factories, or
  `main.rs`'s single-user call sites — `with_user_budget` is additive on
  `HttpClient`, and every existing caller simply never calls it.
- A user with no per-user rpm/concurrency configured is provably unaffected
  (no `UserSlot` is ever created for them).

### Negative / neutral

- Per-user budgets are **in-process only** — unlike the endpoint-wide gates,
  `UserSlot` is not mirrored to the cross-process shared-state file
  (ADR-0144), so two `skutter` processes sharing both a literal key *and* a
  user would each apply that user's cap independently rather than jointly.
  Acceptable for v1: multi-user mode is an embedder-library API today
  (ADR-0147), typically one process per deployment; cross-process per-user
  sharing is a well-scoped future addition if a multi-process multi-user
  deployment needs it.
- `ThrottleStatus` (`client/status.rs`) does not surface which user's slot is
  binding — it still reports whichever of the endpoint/model gates is most
  saturated, same as before this ADR. A saturated user slot is invisible to
  the TUI's throttle indicator. Deferred, mirroring ADR-0140's own choice not
  to extend `ThrottleStatus` beyond what it already covered.
- No env-var override for a per-user cap (there is no per-user env in
  multi-user mode at all — keys/config come from `UserProviderStore`, never
  `std::env`, by ADR-0147's own design), so this isn't a new gap.

## Alternatives considered

- **Widen `pool_key` with a user discriminator** (ADR-0147's option a).
  Rejected: see "Why not widen the pool key" above — it would split the
  endpoint's single 429 cool-down per user, breaking the "every caller of a
  throttled key backs off together" property ADR-0111 exists to provide.
- **Thread `user`/`user_rpm`/`user_concurrency` as new `execute_with_retry`
  parameters** (the literal ADR-0140 shape, applied to users). Rejected in
  favor of attaching the budget to the `HttpClient` handle itself
  (`with_user_budget`): the parameter approach would touch all three wire
  client structs, their `*_factory` functions, and every call site
  (`main.rs`, `mcp/http.rs`), for a value that is already naturally
  per-handle (a user's `Llm` instance already gets its own `HttpClient` clone
  in `resolve_for_user`) rather than per-call.
- **Mirror the per-user budget into the cross-process shared-state file**
  (ADR-0144) now. Rejected for v1 scope: multi-user mode is single-process
  today (ADR-0147); extending the shared-state file format to a third
  dimension (endpoint × key × user) before a real multi-process multi-user
  deployment exists would be speculative.

## References

- Issue #632: per-user RPM/concurrency on a shared literal API key (part of
  the deferred-work-ledger epic, #624)
- [ADR-0147](0147-multi-user-mode-embedder-api.md): multi-user embedder API —
  "Consequences" is where this gap was first recorded
- [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): the pool
  key (`base_url` + `sha256(api_key)`) this gate layers above without
  changing
- [ADR-0140](0140-per-model-concurrency-cap-layered-on-endpoint-cap.md): the
  per-model admission gate this ADR's `UserSlot`/ordering mirrors
- [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md): the
  endpoint-wide adaptive pacing + 429 cool-down this ADR deliberately leaves
  un-scoped per user
- [ADR-0144](0144-file-backed-shared-endpoint-state-across-instances.md)/[ADR-0156](0156-normalize-and-stabilize-the-endpoint-pool-key.md):
  the cross-process shared state this ADR's per-user gate does *not* extend
  to (v1 gap, see Consequences)
