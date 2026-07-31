# 0141. Wire-visible throttle transitions: `OutEvent::Throttle`, engine-global not per-session

- Status: Accepted
- Date: 2026-07-31
- Amends: [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md) (adds a wire-facing signal for the pool's already-existing state), [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md) (surfaces the AIMD pacing gate's countdown)

## Context

`entanglement-provider`'s `HttpClient` tracks per-endpoint throttle state — a
429 cool-down window, the adaptive pacing gate's slowdown, the in-flight
concurrency cap (ADR-0050/ADR-0111) — but before this change that state was
visible **only to the TUI**: `App` holds the `HttpClient` directly
(`tui/mod.rs`) and `input_panel.rs` polls `throttle_status()` every frame to
render `⚠ api.z.ai throttled · retry 8s · 2/3`. A stdio (`run`) or WebSocket
(`serve`) head — anything that isn't the TUI — saw nothing: `Status::Thinking`
for the entire stall, which can run up to `rate_limit_max_elapsed` (≈15
minutes) under a persistent 429.

Two smaller, related gaps closed alongside:

1. `RateLimiter::PaceState::next_slot` (the AIMD gate's next reserved slot) was
   computed on every `acquire` but never exported — `ThrottleStatus` carried
   only a `penalized: bool`, so even the TUI's own "pacing" label showed no
   countdown.
2. `HttpClient::throttle_status()` narrows to the single *most-throttled*
   endpoint (by design, for a compact one-line indicator) — useful for the
   TUI's bottom bar, but a caller that needs to track *every* endpoint's own
   posture over time (to detect a per-endpoint transition without one busy
   endpoint masking another) had no snapshot to read.

## Decision

**Part A — export the pacing countdown (`entanglement-provider`).**
`ThrottleStatus` gains `next_request_in: Option<Duration>`, computed in
`EndpointState::status()` as `next_slot.checked_duration_since(now)`,
surfaced only while `penalized` is true (an unpenalized gate's `next_slot` is
usually already in the past — not throttle-relevant). A new
`HttpClient::throttle_statuses() -> Vec<ThrottleStatus>` returns every
resolved endpoint's snapshot (not just the most-throttled), and
`ThrottleStatus::is_throttled()` — previously a private `EndpointState`
method — moved onto `ThrottleStatus` itself so a caller outside the crate can
classify a `throttle_statuses()` snapshot the same way
`throttle_status()` does internally. The TUI's `throttle_label` now renders
`⚠ api.z.ai pacing · next 1.2s · 2/3` instead of the bare `pacing`.

**Part B — a wire-visible, engine-global lifecycle event.**
`OutEvent::Throttle { endpoint, throttled, in_flight, cap, retry_in_ms,
pacing_in_ms }` joins the protocol as a **point-in-time, no-`seq`** event —
`session()` returns `None` for it, exactly like `OutEvent::McpChanged`/
`BashChanged`. This is the load-bearing call: the provider's resilience pool is
explicitly **per-endpoint, not per-session** (ADR-0050's whole point is that
one throttled endpoint never blocks another, and many sessions/sub-agents
share one endpoint's budget) — so the event names the endpoint, not a
session. Milliseconds (`u64`), not `Duration`, so the wire representation
carries no serde-internal shape.

A new runtime module, `entanglement-runtime::throttle`
(`spawn_throttle_responder`), polls `HttpClient::throttle_statuses()` every
500ms and emits `OutEvent::Throttle` **only on a transition** — classified as
`AtRest | Busy | Pacing | Backoff` (mirroring the TUI label's precedence:
an active cool-down wins over pacing, which wins over a bare saturated cap).
A held stall re-polls silently; only entering/leaving a class re-emits, so a
15-minute 429 parks quietly on the wire instead of one event every 500ms.
Spawned and `.abort()`'d in `main.rs` alongside the other engine-global
runtime responders (`mcp::spawn_mcp_responder`, `bash_live::spawn_bash_responder`)
— it needs no graceful drain (unlike the persistence subscriber), so an abort
at shutdown is sufficient.

`Holly::emit_throttle` is the emission seam, mirroring `emit_bash_changed`/
`emit_mcp_changed`: no `seq` counter touched, a plain broadcast.

Every head picks this up automatically through the ordinary `OutEvent`
fan-out: `serve` relays raw frames with no per-variant match, so nothing there
needed to change. The stdio `run --format text` head's exhaustive match
(`run.rs`) renders it in full (`⚠ https://api.z.ai/v4 throttled · retry 8s ·
2/3` / `✓ … throttle cleared`) — the whole point, since it previously had no
signal at all. The TUI's per-session reducer (`tui/session_view/reducer.rs`)
gained a `false` arm purely for match exhaustiveness — the event never reaches
it (`sessions.handle_out_event` short-circuits on `event.session().is_none()`
before dispatch, same as `McpChanged`/`BashChanged`), since the TUI already
renders throttle state directly via its own `HttpClient` handle and gains
nothing from double-plumbing through the wire event it emits for other heads.

## Consequences

- **(+)** A remote (stdio/WS) head now sees a 429 cool-down or pacing
  slowdown, with a countdown, instead of an opaque multi-minute `Thinking`
  stall.
- **(+)** The TUI's own "pacing" indicator gains the countdown Part A exports,
  for free — the same `ThrottleStatus` field both consumers read.
- **(+)** Transition-only emission keeps a long stall from spamming the wire —
  the acceptance bar from the originating issue (#517).
- **(+)** No new per-session state or `AgentState` variant: the resilience
  layer's per-endpoint nature (ADR-0050) stays honest instead of being
  awkwardly projected onto whichever session happens to be mid-request when a
  poll fires.
- **(−)** A head that wants to correlate a throttled *endpoint* back to the
  *session(s)* currently blocked on it has no protocol-level join — it would
  need to track `ModelChanged`'s `provider`/`model` per session and infer the
  endpoint itself. Accepted: no current head needs this (the TUI's existing
  indicator is deliberately not session-scoped either), and forcing the join
  would require threading endpoint identity through `ModelChanged` for a use
  case nothing yet has.
- **(−)** One more poller task per running process (500ms tick, cheap: a
  handful of mutex-guarded reads over however many endpoints are in the
  pool) — bounded by the number of distinct provider endpoints in use, never
  by session count.
- **(−)** One more exhaustive-match arm at every existing `OutEvent` call site
  (`run.rs`, `reducer.rs`, `protocol.rs`'s `session()`/`seq()`); compiler-enforced,
  the same tradeoff every prior protocol addition has taken.

## Alternatives considered

- **Attach `session` to `Throttle` (the issue's initial sketch).** Rejected:
  there is no principled single session to name — the endpoint's queue can
  be shared by an arbitrary number of concurrent sessions/sub-agents at the
  moment of a transition, and picking "whichever session's request tripped
  the 429" would misrepresent every *other* session waiting on the same
  gate. `McpChanged`/`BashChanged` already established the engine-global,
  session-less shape for exactly this kind of pool-wide state.
- **A new `AgentState::RateLimited` per-session state**, as the issue floated.
  Rejected for the same reason: `AgentState` is a *session's* lifecycle word,
  and the resilience layer is deliberately not per-session (ADR-0050). Folding
  a per-endpoint fact into a per-session enum would either apply it to every
  session sharing the endpoint (semantically wrong — most aren't mid-request)
  or only the one that happens to be polled-and-blocked (arbitrary and racy).
  The lifecycle event is the right granularity; a head that wants a
  session-local hint can still derive "my request is slow" from the ordinary
  turn-latency it already observes.
- **Emit on every poll tick, not just transitions.** Rejected outright by the
  originating issue's acceptance criteria and by the ≈15-minute worst-case
  429 window: at 500ms that's ~1800 events for one stall, all but the first
  and last carrying no new information.
- **Surface queue depth** (how many callers are waiting behind the
  concurrency semaphore, beyond `in_flight`/`cap`). Deferred: `tokio::Semaphore`
  doesn't expose waiter count, and adding a side-counter purely for this
  display purpose felt like scope creep beyond what #517 asked for — the
  existing `in_flight == cap` "busy" signal already tells a head "this
  endpoint is at capacity," just not by how much it's oversubscribed.
