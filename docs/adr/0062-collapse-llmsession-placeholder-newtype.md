# 0062. Collapse the `LlmSession` placeholder newtype

- Status: Accepted
- Date: 2026-07-14
- Resolves the `LlmSession` sub-decision of [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) (which kept the newtype); builds on the seam inversion of [0053](0053-invert-core-provider-seam.md). Issue #195, epic #190.

## Context

`LlmSession` was a newtype around `Box<dyn Llm>` that delegated `stream` straight
through and exposed an `inner_mut` accessor. The docs sold it as a
"provider-owned session/connection handle carrying per-session retry accounting /
rate-limit budget" — but no such state was ever there:

- Pool/retry/rate-limit state is **per endpoint**, held in the provider's
  `HttpClient`/`EndpointPool` keyed by `(base URL, api-key hash)` since #217
  ([0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)). Sessions
  talking to the same endpoint deliberately **share** one budget so a throttled
  endpoint doesn't starve its siblings.
- `inner_mut` had no callers.
- After [0053](0053-invert-core-provider-seam.md) inverted the core↔provider seam,
  `LlmSession` moved into `entanglement-provider` (the crate that owns the
  resilience state), so the fill-or-collapse choice became entirely
  provider-internal — no cross-crate seam to negotiate.

An embedder reading the docs would look for per-session state that isn't there.

## Decision

**Collapse the newtype.** A session owns its LLM backend as a plain
`Box<dyn Llm>`; `LlmFactory` becomes `Arc<dyn Fn() -> Box<dyn Llm> + Send + Sync>`
and `Session::llm` is a `Box<dyn Llm>`.

Giving `LlmSession` a *session-scoped* retry budget / last-`Retry-After` was
rejected: that would re-fragment exactly what #217 unified. Resilience is a
property of the **endpoint**, not the conversation — two sessions on one endpoint
must share the budget, and one session may (via a profile-pinned model) target a
different endpoint per turn. There is no honest per-session connection state, so
the wrapper was pure indirection.

The newtype should be **re-introduced only when genuinely per-session state
arrives** — e.g. a conversation-scoped token budget, a session-pinned model
override resolved once, or sticky-endpoint affinity. Until then, the most direct
expression (KISS) is the boxed trait object the factory already produces.

## Consequences

### Positive

- The type system stops advertising state that doesn't exist; the docs match the
  code.
- One less layer between the turn loop and the backend; `dyn Llm`'s methods are
  callable directly on the trait object (the `Llm` import is no longer needed at
  the call site).
- No behavior change: `stream` was a straight delegation.

### Negative / neutral

- Re-introducing a per-session handle later is a mechanical change to `LlmFactory`
  + `Session::llm` + call sites (as this collapse was, in reverse). Cheap to
  reverse, which is the whole argument for not carrying the empty shell now.

## References

- Issue #195: `LlmSession` was a placeholder — no per-session provider state
- [0050](0050-per-endpoint-connection-pool-retry-rate-limit.md): per-endpoint
  pool/retry/rate-limit — where the resilience state actually lives (#217)
- [0053](0053-invert-core-provider-seam.md): moved `LlmSession` into the provider
  crate, making this decision provider-internal
- Part of epic #190 (provider seam + per-endpoint pool)
