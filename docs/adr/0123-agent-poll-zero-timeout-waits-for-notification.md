# 0123. `agent_poll` `timeout_secs: 0` waits for the child's notification

- Status: Accepted
- Date: 2026-07-21

## Context

[ADR-0026](0026-async-subagent-spawn-and-poll.md) introduced `agent_poll` with a
bounded wait: `tokio::time::timeout(Duration::from_secs(timeout_secs),
wait_complete(…))`. `MAX_TIMEOUT_SECS = 600` deliberately prevents an indefinite
park — the model is expected to poll again rather than block forever.

The blocking sibling tool `agent` ([ADR-0033](0033-agent-tool-family-and-blocking-agent.md))
has *no* such bound: it parks on the child's `Done` (and thus on the same
`wait_complete` / `collect_child_answer` path) and only returns when the child
completes. So the runtime already proves that waiting without a caller-side bound
is hang-safe — `wait_complete` breaks when the watch sender drops, and the
detached launch watcher always drops the sender on the child's `Done`.

But there was no way to get that unbounded wait through `agent_poll`: passing
`timeout_secs: 0` made `tokio::time::timeout` fire **immediately**, so the call
returned the "still running" status without ever waiting. `0` was a degenerate,
no-op value with no useful meaning. (`parse_input` already preserved a literal
`0`: `0.min(MAX_TIMEOUT_SECS) == 0`.)

## Decision

`timeout_secs: 0` is the sentinel for **"wait for the child's completion
notification, no caller-side bound."** `run_agent_poll` branches on `0` and calls
`wait_complete` directly, skipping the `tokio::time::timeout` wrapper — exactly
the path the blocking `agent` tool takes. Positive values keep the existing
bounded behavior and the `MAX_TIMEOUT_SECS` cap.

This is purely a wait-branching change — no parsing change, no new field, no new
tool. The `agent_poll` input shape is unchanged.

### Amends

[ADR-0026](0026-async-subagent-spawn-and-poll.md) (the bounded-wait contract) is
narrowed: `timeout_secs: 0` is now the explicit carve-out from the
"never-park-indefinitely" rule it established. ADR-0026 itself is immutable and
unchanged; this ADR records the carve-out.

## Why a sentinel, not a new tool or a flag

- **`0` was dead.** It previously meant "return immediately," which no caller
  has a reason to ask for (an immediate still-running return gives the model no
  information it didn't already have). Reclaiming a dead value needs no new
  surface area.
- **The safe path already exists.** The blocking `agent` tool already parks on
  the same `wait_complete` path with no bound and is known hang-safe; this only
  generalizes that path to the poll entry point.
- **No input-shape churn.** A separate `block: bool` field would be vaguer
  ("block on what?") and a new tool (`agent_wait`?) would duplicate `agent_poll`
  almost entirely. The sentinel keeps the schema identical.

## Consequences

- **Positive:** a model can opt into a guaranteed join through `agent_poll`
  without spawning a second `agent` call; symmetric with the blocking `agent`
  tool's wait semantics.
- **Negative / neutral:** a model that sends `0` can hang its own turn until the
  child finishes. This is the explicit opt-in the caller asked for and matches
  `agent`'s behavior; acceptable. Positive timeouts still cannot.

## Alternatives considered

- **`timeout_secs: -1`** (negative sentinel) — would require widening the field
  from `u64` to `i64` across parse/schema/UI-render, a larger type churn for no
  behavior gain over reclaiming `0`.
- **A separate `block: true` field** — vaguer (block on what?), a new field vs.
  reclaiming a dead value, and forces the model to set two things instead of one.
- **Cap `0` to `MAX_TIMEOUT_SECS`** — defeats the purpose: the whole point is the
  unbounded wait.

## References

- [ADR-0026](0026-async-subagent-spawn-and-poll.md): `agent_poll` bounded-wait contract (this amends the `0` case)
- [ADR-0033](0033-agent-tool-family-and-blocking-agent.md): blocking `agent` tool — the existing unbounded-wait precedent this generalizes
