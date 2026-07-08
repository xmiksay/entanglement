# 0023. Sub-agent spawn recursion / fan-out limits

- Status: Accepted
- Date: 2026-07-09

## Context

[ADR-0022](0022-subagent-spawn.md) gave the engine a spawn path (`InMsg::Spawn`
+ the runtime `spawn_agent` tool) but explicitly deferred any bound on the spawn
*tree*. As shipped, a sub-agent could itself call `spawn_agent`, and any session
could spawn arbitrarily many children. Each session is capped by `MAX_TURNS`
(core, `session.rs`), but that bounds one session's turn loop — not the breadth
or depth of the tree of sessions it spawns. A model stuck in a "delegate
everything" loop could fan out unboundedly. This is issue #76.

## Decision

Bound the spawn tree in the **runtime tool executor** — the same layer that
already intercepts `spawn_agent` (ADR-0022) — with two independent limits:

- **Depth** (`MAX_SPAWN_DEPTH = 3`): the root (user-initiated) session is depth
  0; a spawn creates a child at `depth(parent) + 1`. A spawn whose child would
  exceed the cap is refused. Bounds recursion.
- **Per-root fan-out** (`MAX_SPAWNS_PER_ROOT = 16`): a cumulative count of
  sub-agents spawned anywhere beneath a root, keyed by the root's `SessionId`.
  **Never decremented** — sequential spawns count too, so a session cannot dodge
  the cap by letting each child finish before starting the next. Bounds breadth.

A new `subagent::SpawnGuard` owns both. It lives in the tool executor's
single-threaded event loop (`tool_runner.rs`), so the check is race-free and
needs no locking:

1. it folds child→parent links from every `OutEvent::SessionStarted`
   (`record_start`) — the executor already sees these in order, and a session's
   `SessionStarted` always precedes any `ToolExec` it emits;
2. on a `spawn_agent` `ToolExec`, `try_spawn(parent)` walks the parent chain for
   depth and root, applies both limits, and on approval charges the root's
   budget.

On refusal the executor does **not** start a child; it replies to the parent's
parked tool call with a plain `ToolResult` carrying a clear message (e.g. "max
spawn depth (3) reached … Do the work directly"). The parent's turn loop folds
it in as an ordinary `ToolOutput` (#58) and continues — no core change, no
special protocol variant, symmetric with the existing permission-deny path.

## Consequences

### Positive

- The spawn tree is bounded on both axes; a runaway delegation loop is refused
  with an actionable message rather than exhausting sessions/tokens.
- Zero core surface: the guard is pure runtime state folded from events already
  on the outbox, consistent with the three-layer split (ADR-0006/0010). Core
  still has no notion of a "child session".
- The refusal is an ordinary tool result, so every head (stdio, TUI, future WS)
  surfaces it with no new handling.

### Negative / neutral

- The limits are fixed constants, not yet configurable per profile or via env.
  Sufficient for v1; a config knob can follow if a real workload needs it.
- The per-root budget is cumulative and never reclaimed, so a very long-lived
  root session eventually exhausts it even across unrelated subtasks. Acceptable:
  16 sub-agents per root is generous, and a fresh root resets it.
- Depth/root are derived from `SessionStarted` events the executor may in theory
  drop if it lags (broadcast `Lagged`); an under-count would only ever *allow*
  slightly more, never wrongly refuse. The existing lag warning already flags it.

## Alternatives considered

- **Track limits in core's supervisor** (it owns the real `parent_links`).
  Rejected: pulls policy back into core against ADR-0006; the runtime already has
  every fact it needs from the outbox.
- **Depth-only or fan-out-only.** Depth alone lets a session spawn thousands of
  shallow children; fan-out alone lets a narrow-but-infinitely-deep chain form.
  Both axes are cheap and cover distinct failure modes.
- **Decrement the budget when a child ends.** Would let a sequential spawner run
  forever, defeating the fan-out bound; the cumulative count is deliberate.
- **Refuse by killing the parent turn.** Rejected: a soft refusal that lets the
  model recover (do the work itself) is friendlier and matches the deny path.

## References

- Issue #76: runtime recursion / fan-out limits for sub-agent spawn
- [ADR-0022](0022-subagent-spawn.md): sub-agent spawn (deferred these limits)
- [ADR-0021](0021-hierarchical-session-model.md): hierarchical session model
- [ADR-0006](0006-core-dependency-hygiene-gate.md): core dependency hygiene
