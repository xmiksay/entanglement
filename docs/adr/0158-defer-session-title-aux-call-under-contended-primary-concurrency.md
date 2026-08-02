# 0158. Defer the session-title aux call when the primary model's concurrency cap is contended

- Status: Accepted
- Date: 2026-08-02

## Context

[ADR-0154](0154-per-purpose-auxiliary-models.md) fires the session-title
generator's aux LLM call *concurrently* with the main turn's own first call —
by design, so a title appears without adding latency to the turn the user is
actually waiting on. That design assumed the two calls admit independently.

They don't, when the aux purpose has no pin ([ADR-0154]'s documented common
case — falls back to the primary `LlmFactory`) and the primary model carries a
tight [ADR-0140](0140-per-model-concurrency-cap-layered-on-endpoint-cap.md)
per-model cap. With a cap of 1 (z.ai's documented `glm-4.7-flash` tier), the
main turn's own request holds the model's only permit for the whole turn; the
concurrently-fired aux call then blocks on `ModelSlot::semaphore` until that
permit frees — every time, not occasionally. This reproduces with **zero
user-set config**: `concurrency: 1` is catalog data, not something a user
dialed in, and a user with one model configured has no pin to route the aux
call elsewhere. Reported (#589) as the engine "waiting for something" on a
session's first prompt — bounded (the permit does free), but a silent,
avoidable serialization of two calls the concurrent design intended to be
independent.

## Decision

**Judge contention risk before firing, and sequence instead of racing when
it's guaranteed.**

`Catalog` gains `effective_concurrency(provider, model) -> Option<usize>`
(`entanglement-provider/src/catalog.rs`): the model's own `concurrency` when
set, else its provider's endpoint-wide `concurrency`, else `None` — a
one-shot snapshot lookup, distinct from `model_concurrency_resolver`'s
resolver-closure shape built for a live client.

`AuxLlmRegistry` (`entanglement-runtime/src/aux_llm.rs`) gains
`concurrency_cap(purpose) -> Option<usize>`, mirroring `resolve`'s own
fallback exactly: the pin's effective cap when one resolves, else the primary
model's — snapshotted once at construction via a new `catalog: Catalog` +
`primary_concurrency: Option<usize>` pair on the registry, so this can never
disagree with which client `resolve` would actually hand back.

`spawn_session_title_generator` (`entanglement-runtime/src/session_title.rs`)
checks `concurrency_cap(SessionTitle) <= 1` on each `Prompt`. Below that
ceiling, contention is *possible but not certain* (more than one permit means
the aux call may slot in for free) and firing stays concurrent, unchanged.
At the ceiling, contention is **guaranteed** — the main turn's own request is
certain to hold the model's only permit — so the generator instead subscribes
to the outbound event stream immediately (before spawning, to close the race
against an already-in-flight turn) and waits for that session's first `Done`
or `Error` before making its aux call, bounded by a 300s safety-net timeout
so a turn that never settles (parked on approval, `Stop`) doesn't strand the
title forever.

## Consequences

### Positive

- Fixes the reported symptom with no user action required: a single-model,
  no-pin setup on a `concurrency: 1` model no longer visibly stalls the first
  prompt on an aux call racing the main turn for the same permit.
- The common case (cap > 1, or a pin to an unrelated model) is provably
  unaffected — `concurrency_cap` returns `None` or a cap above the ceiling,
  and the generator's existing concurrent-fire path runs unchanged.
- The deferred path still degrades to "no title" rather than hanging: the
  wait is bounded, and a lagged/closed broadcast falls through to firing the
  aux call anyway (matches the module's existing best-effort posture).

### Negative / neutral

- On a contended model, the title now visibly appears *after* the turn
  completes rather than mid-turn — a latency regression against the
  concurrent design's original intent, but strictly better than the silent
  block it replaces (same wall-clock floor, no wasted racing).
- `AuxLlmRegistry::new` gained two parameters (`catalog`, `primary_concurrency`)
  — mechanical, touches every call site (`main.rs`, unit tests, the
  `session_title` integration test).
- The contention check only looks at the resolved model's own cap; it does
  not account for other concurrent traffic against the same endpoint (spawned
  sub-agents, other sessions) already occupying permits. Acceptable: that
  traffic contends for the *main turn's* permit too, so the main turn's own
  wait already reflects it, and the aux call's relative deferral decision
  doesn't need to re-derive it.

## Alternatives considered

- **Carve out a dedicated `aux_concurrency` reservation in the catalog**, so
  aux traffic gets its own slice of a model's permits instead of contending at
  all. Rejected for v1: only helps when a model's cap is already > 1 (a
  reservation of 0 from a cap of 1 gives the primary turn nothing); the
  reported failure is specifically the cap-of-1 case, where the only
  correctness-preserving move is not admitting a second concurrent request
  against the provider's real documented ceiling in the first place. A
  worthwhile v2 for the cap > 1 case if it turns out to matter in practice.
- **A separate semaphore/permit pool for aux traffic**, bypassing the
  per-model cap entirely. Rejected: at cap 1 this would admit 2 concurrent
  requests against a documented 1-in-flight provider limit, trading a
  perceived engine stall for a real provider 429 — worse, not better.
- **Skip session-title generation entirely** on a contended model rather than
  deferring. Rejected: throws away a working feature (the title still
  generates correctly, just later) for a config shape (a tight model cap)
  that's common enough (z.ai's Flash tier) to make "no title, ever, on this
  model" a bad trade for a bounded, already-tolerable delay.
- **Lower `glm-4.7-flash`'s default `concurrency` cap in the catalog.**
  Rejected: it's z.ai's own documented real ceiling, not an invented
  conservative default — raising it would 429-storm the provider, matching
  [ADR-0140]'s reasoning for not inventing per-model numbers.

## References

- [ADR-0154](0154-per-purpose-auxiliary-models.md): the aux-model registry and
  the session-title generator's concurrent-fire-on-first-`Prompt` design this
  amends.
- [ADR-0140](0140-per-model-concurrency-cap-layered-on-endpoint-cap.md): the
  per-model concurrency cap this ADR reads to judge contention risk.
- Issue #589 (part of the #588 pre-release audit umbrella).
