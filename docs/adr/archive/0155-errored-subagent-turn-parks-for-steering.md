# 0155. An errored sub-agent turn parks the parent for steering, not unblocks it

- Status: Accepted
- Date: 2026-08-02
- Amends: [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md) ("a saturated endpoint fails a sub-agent's turn rather than hang its parent" — the *turn* still fails, but that no longer unblocks the parent), [ADR-0123](0123-agent-poll-zero-timeout-waits-for-notification.md) (the terminal-`Done` assumption `wait_complete`/`collect_child_answer` relied on), [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md) (the sponsored build wait)

## Context

The repro chain: a plan agent's accepted plan sponsors a `build` child
([ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md)); the build
agent delegates to a further sub-agent via the blocking `agent` tool
([ADR-0033](0033-agent-tool-family-and-blocking-agent.md)). When the deepest
child's turn ends in error — out of tokens, or the endpoint's 429 budget
exhausted per [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md)
— the error cascaded silently up the whole chain: the build agent's `agent`
call resolved with `"sub-agent ended with error: …"` as an ordinary tool
result, the build agent concluded (possibly reporting success on top of an
unfinished build), and the sponsored wait unblocked the plan agent in turn.

The cause: `collect_child_answer` (`entanglement-runtime/src/subagent.rs`,
shared by the blocking `agent` tool and the ADR-0138 sponsored-build wait)
recorded `OutEvent::Error` but broke out of its watch loop on the child's
`Done` — and the engine's `emit_turn_error` (`entanglement-core/src/session/emit.rs`)
fires *both* `Error` and `Done` for a turn that ended in error, exactly
paralleling `emit_turn_done` for a clean finish (#434 made that pairing
deliberate: every termination path emits the same `Error`-then-`Done` or
`Done`-alone shape, precisely so a one-shot head can always exit on `Done`).
That symmetry is right for a one-shot head, but wrong for a *watcher* deciding
whether the child is actually finished: it can't tell "the turn ended, nothing
more is coming" from "this turn ended, but the session is still live and could
run another." The same terminal-`Done` assumption sat in `agent_spawn`'s
detached launch watcher and, transitively, in `agent_poll`'s `wait_complete`
(both consume the same `AgentRegistry` entry `collect_child_answer` fills).

ADR-0111 framed the fix for the underlying 429-hang bug as "a saturated
endpoint should fail a sub-agent's turn, not stall it" — correct for the
*provider* layer, whose only two options are "block forever" or "surface an
error and let the turn end." But the *runtime* layer sitting above it had a
third option ADR-0111 didn't consider: a child session that ends a turn in
error is not dead. It's an ordinary idle session, exactly as steerable as one
that finished cleanly — prompting it "continue" starts a fresh turn. Treating
its `Done` as final discards that option and forces a irrecoverable failure
onto the whole ancestor chain for what may be a transient, retryable turn.

## Decision

`collect_child_answer` keeps watching past a `Done` that carries no usable
answer: if a turn ends with empty accumulated text *and* it surfaced an
`Error`, the per-turn `text`/`error` state is cleared and the loop continues
instead of breaking. The wait now ends only on:

- a turn's `Done` that *does* carry a usable answer (unchanged — the common
  case: most turns don't error),
- the child's `SessionEnded` or `SessionHibernated` (the child is genuinely
  gone), or
- a lagging/closed watcher (defensive — a missed event still can't park the
  parent forever, unchanged from before).

Each time the wait re-arms past an errored turn, the parent is told why it's
still parked: `collect_child_answer` emits an `OutEvent::Error` on the
**parent** session (`holly.emit_for_session`, a normal seq'd content event —
no new wire type) naming the child and explaining that it is still alive and
should be steered (e.g. prompted to "continue"). The parent's `AgentState`
stays `WaitingAgent` throughout — this is not a new lifecycle state, just an
explanatory note riding the existing wait.

`collect_child_answer` gained two parameters, `holly: &Holly` and
`parent: &SessionId`, to emit that note; both existing call sites
(`subagent::launch`, `propose_plan::launch_sponsored_build`) already had both
in scope.

Because `agent_spawn`'s detached watcher and the blocking `agent` tool share
the exact same `collect_child_answer` call, and `agent_poll`'s
`wait_complete` only ever observes the `AgentStatus::Complete` that watcher
publishes, this one change gives all three consumers — `agent`, `agent_spawn`
+ `agent_poll`, and the ADR-0138 sponsored build — the same semantics with no
separate fix needed at each site.

## Consequences

### Positive

- A child that errors transiently (budget exhaustion, a flaky endpoint) no
  longer forces an irrecoverable failure onto its whole ancestor chain — the
  parent stays parked, the user (or the parent agent itself, on a future pass)
  can steer the child to a genuine conclusion.
- No wire protocol change: the "steer me" signal rides the existing
  `OutEvent::Error` content-event shape and the existing `WaitingAgent`
  state, so no head needs new rendering code to see it (though a head *could*
  special-case it later).
- `agent`, `agent_spawn`/`agent_poll`, and the sponsored build wait all get
  the fix from one shared function.

### Negative / neutral

- A child stuck in a genuine error loop (repeatedly erroring on every
  "continue") now parks its parent indefinitely instead of failing fast. This
  is the deliberate trade this ADR makes — the old behavior silently produced
  a *wrong* answer (success reported on a failed build) rather than a slow
  one. `SessionEnded`/`SessionHibernated` (session close, TTL, or a `Stop`
  cascaded to the child) still ends the wait.
- The blocking `agent` tool's task is still not registered with
  `CancelRegistry` (a parent `Stop` does not abort it — by design, see the
  comment in `subagent::launch`, so the child's eventual answer stays
  collectable via `agent_poll`); this ADR doesn't change that asymmetry with
  the sponsored-build wait, which *is* `CancelRegistry`-registered
  ([ADR-0145](0145-one-plan-tool-file-backed-plans-and-blocking-review-loop.md)/#513).
- No new `AgentState` variant or wire field — a future iteration could add a
  dedicated "waiting-agent-errored" detail (attention-bell-worthy, unlike
  plain `WaitingAgent`) if the plain informational `Error` event proves too
  easy to miss in practice.

## Alternatives considered

- **A new `AgentState`/wire field for "parked on an errored child."** Rejected
  for this change: touches every exhaustive `AgentState` match across heads
  (ADR-0139's precedent) for a signal that the existing `Error` content event
  already carries adequately. Left as a documented option above if the
  lighter-weight signal proves insufficient.
- **Bound the re-wait with a retry cap (like ADR-0111's `rate_limit_max_elapsed`).**
  Rejected: unlike a 429 window, there's no natural bound on how long a human
  takes to notice and steer a parked child — an arbitrary cap would just
  reintroduce the original bug (unblock the parent on a still-recoverable
  child) at a different timescale.
- **Fail fast, unchanged from before, but surface the failure more loudly.**
  Rejected: louder surfacing doesn't fix the root problem — the parent still
  concludes on top of a failed child before the user can react.

## References

- Issue #562.
- [ADR-0111](0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md): the
  429-retry-until-clear behavior whose "fail the turn" framing this narrows to
  the provider layer only.
- [ADR-0123](0123-agent-poll-zero-timeout-waits-for-notification.md): the
  unbounded-wait precedent this builds on (`wait_complete`/`collect_child_answer`
  are still hang-safe — they resolve on `SessionEnded`/`SessionHibernated`
  instead of a bare `Done`).
- [ADR-0138](0138-sponsored-build-child-and-propose-plan-cycle.md): the
  sponsored build wait that shares `collect_child_answer`.
- [ADR-0139](0139-visible-session-states-working-and-waitingagent.md):
  `WaitingAgent`, unchanged by this ADR — still not attention-bell-worthy.
- #434 (`entanglement-core/src/session/emit.rs`): the `emit_turn_error`/
  `emit_turn_done` pairing whose `Error`-then-`Done` shape is the terminal-`Done`
  assumption this ADR corrects for at the watcher, not the emitter.
