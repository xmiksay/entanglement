# 0033. `agent_*` tool family — rename `spawn_agent` → `agent_spawn`, add blocking `agent`

- Status: Accepted
- Date: 2026-07-10

## Context

Sub-agent orchestration grew two runtime-owned tools across
[ADR-0022](0022-subagent-spawn.md) and
[ADR-0026](0026-async-subagent-spawn-and-poll.md): `spawn_agent` (launch,
non-blocking, returns a handle) and `agent_poll` (join, bounded wait). Their
names don't read as one family — `spawn_agent` sorts away from `agent_poll` in a
tool listing, and nothing signals that they compose. This is the first change of
the agents/skills/system-prompt epic (#111); every later change should build on
the final names, so the rename lands first.

Two usage shapes exist in practice:

- **Fan-out** — launch several sub-agents, let them run concurrently, then join
  each. `agent_spawn` + `agent_poll` (ADR-0026) is exactly this.
- **Single delegation** — "ask an explore agent this one thing and give me the
  answer." With only the launch/poll pair the model must call `spawn_agent`,
  then `agent_poll` in a follow-up turn — a two-call dance for the common case.

## Decision

Unify the tools under one `agent_*` family and add a blocking variant:

| tool | input | behavior |
| --- | --- | --- |
| `agent_spawn` | `{ agent, prompt }` | **rename of `spawn_agent`** — non-blocking, returns the child handle (`agent_id`) immediately (ADR-0026). |
| `agent_poll` | `{ agent_id, timeout_secs }` | unchanged — bounded wait for a spawned child's answer. |
| `agent` | `{ agent, prompt }` | **new** — blocking: spawns and waits internally for the child's final answer, returns it directly as the `ToolOutput`. |

A consistent prefix means the model sees the family as one unit next to
`ask_user`/`load_skill`, and tool listings sort together. Both spawning tools
carry (once #119 lands) the spawnable-agent roster in their **tool
descriptions** with the `agent` input constrained to the allowed enum — the model
learns "who can I spawn" at the call site, not from system-prompt prose.

### No back-compat alias

Tool names are opaque strings in the wire protocol; nothing persists them across
versions except session JSONL logs, which replay fine (the name is just a label
in `ToolExec`/`ToolOutput` records). So `spawn_agent` is renamed outright, no
alias.

### Why `agent` is a separate tool, not a `blocking:` flag on `agent_spawn`

The two return shapes differ — `agent_spawn` returns `agent_id` + a "poll me"
status; `agent` returns the child's final answer. A boolean that flips the return
type forces a vaguer, do-both tool description (the same reasoning that split
`spawn_agent`/`agent_poll` in ADR-0026). Distinct tools keep each description
sharp: `agent` is the ergonomic one-call path for a single delegation;
`agent_spawn` + `agent_poll` remain the fan-out path.

### Implementation

`agent` reuses the exact `agent_spawn` launch path — one shared `launch()` in
`runtime::subagent` parameterized by a `LaunchMode`. `agent_spawn` (`Detached`)
replies the handle immediately, then records the child's answer into the
`AgentRegistry` for a later `agent_poll`. `agent` (`AwaitAnswer`) skips the
immediate reply, parks on the child's `Done`, and folds the answer + elapsed
straight into the `ToolOutput` — an `agent_poll` wait with no caller timeout.
Both are intercepted on `ToolExec` before permission resolution and go through
the **same** `SpawnGuard` depth/budget checks (ADR-0023) and ancestor-permission
clamp (ADR-0024), so refusals — depth, budget, capability, and #119's
`can_spawn`/`spawnable_agents` once it lands — are **identical** for `agent` and
`agent_spawn` (asserted by test).

Because `agent` still records into the registry, a parent `Stop` while parked
unwinds via the existing cancelled tool-result wait (ADR-0017/0018): the child
keeps running and remains collectable via `agent_poll` if the model re-asks with
the handle.

## Consequences

- **Positive:** one coherent tool family; the common single-delegation case is
  one call; fan-out unchanged; refusal parity is structural (shared guard path).
- **Negative / neutral:** three orchestration tools instead of two; `agent`'s
  blocking call re-imposes the per-call parent block that ADR-0026 removed — but
  only for that one call, and by explicit model choice (fan-out stays available).

## Alternatives considered

- **`blocking:` flag on `agent_spawn`** — rejected above (vaguer description,
  return type depends on an argument).
- **Keep `spawn_agent`, add an alias** — needless surface; tool names don't
  persist across versions, so a clean rename costs nothing.
- **`agent_cancel { agent_id }`** — a future destructive op (kill an outstanding
  child) fits the same `agent_*` scheme, but it is a distinct authorization gate
  and is **out of scope** here.

## References

- Issue #120: `agent_*` tool family — rename + blocking `agent`
- Epic #111: agents/skills/system-prompt
- [ADR-0022](0022-subagent-spawn.md): sub-agent spawn (original `spawn_agent`; immutable, keeps the old name)
- [ADR-0026](0026-async-subagent-spawn-and-poll.md): non-blocking spawn + `agent_poll` (immutable, keeps the old name)
- [ADR-0023](0023-subagent-spawn-limits.md), [ADR-0024](0024-subagent-permission-gating.md): spawn limits + gating (apply per launch, both variants)
- [ADR-0017](0017-stop-cancels-turn-not-session.md), [ADR-0018](0018-turn-loop-stash-discipline.md): `Stop` cancel semantics + stash discipline
