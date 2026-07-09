# 0026. Non-blocking sub-agent spawn with handle + `agent_poll`

- Status: Proposed
- Date: 2026-07-09

> **Stub.** Captures the decision direction for issue #89. Flesh out
> (precise protocol shape, guard interaction, timeout semantics) before
> implementation; promote to `Accepted` when the change lands.

## Context

[ADR-0022](0022-subagent-spawn.md) shipped the first spawn path: a runtime
`spawn_agent { agent, prompt }` tool issues `InMsg::Spawn`, and the runtime
executor **synchronously relays** the child's final answer back to the parent as
the `spawn_agent` tool's `ToolResult`. Because the parent's turn loop executes
tool calls sequentially and parks on each `ToolResult`
(`session.rs` `run_turn` for-loop → `wait_tool_result`), a `spawn_agent` call
**blocks the parent turn until the child finishes**. Two `spawn_agent` calls in
one assistant turn cannot overlap — the second only starts after the first
child's `Done`. There is also no visibility into how long a spawn took.

This is the concurrency limitation the parent loop imposes; the children
themselves already run on independent tokio tasks. We want fan-out: launch
several sub-agents, let them run in parallel, and collect results when ready.

## Decision (direction)

Split the synchronous `spawn_agent` into a **launch** and a **join**, each a
runtime-owned tool intercepted before permission resolution (same precedent as
ADR-0022):

- **`spawn_agent { agent, prompt } -> agent_id`** — issues `InMsg::Spawn`, then
  returns *immediately* with a handle (the child `SessionId` / UUID) as its
  `ToolOutput`. It does **not** wait for the child's `Done`, so it never blocks
  the parent turn. The runtime keeps watching the child's events in a detached
  task, accumulating its answer + timing keyed by `agent_id`.
- **`agent_poll { agent_id, timeout_secs } -> status + answer?`** — the parent's
  synchronous join point. Blocks up to `timeout_secs` for that specific child;
  returns the child's final answer if it has completed (with elapsed duration),
  or a still-running status on timeout so the model can decide to poll again or
  do other work.

This lets a turn emit several `spawn_agent` calls (each returns a handle fast),
then `agent_poll` each handle — the children run concurrently even though the
parent's tool calls are still dispatched sequentially. Spawn **duration** is
tracked from `Spawn` send to child `Done` and surfaced through `agent_poll`
(and, separately, in the TUI).

**Supersedes** the synchronous answer-relay half of ADR-0022 (the `Spawn`
supervisor branch and spawn limits/gating of ADR-0023/0024 are unchanged and
still apply to each launch).

## Open questions (resolve before Accepted)

- Where the pending-child answer/timing table lives (executor state vs. a new
  registry) and its lifetime / cleanup after a poll drains it.
- `agent_poll` on an unknown / already-drained `agent_id` — error vs. empty.
- Interaction with `SpawnGuard` budgets (ADR-0023) and permission clamping
  (ADR-0024) — unchanged at launch, but confirm poll needs no gating.
- Whether `Stop` on the parent should cancel outstanding un-polled children.
- TUI affordance for in-flight children + their running duration.

## Consequences

- **Positive:** true sub-agent fan-out; non-blocking parent; visible durations.
- **Negative / neutral:** two tools instead of one; the model must remember to
  poll (a launched-but-never-polled child runs to completion unobserved); more
  runtime bookkeeping (handle table, timing).

## Alternatives considered

- **General parallel tool execution in core** (dispatch independent tool calls
  in a turn concurrently). More powerful and benefits all tools, but a much
  larger change to the core turn loop / protocol and stash discipline
  (ADR-0018). Deferred in favor of the narrower, spawn-scoped handle+poll model.
- **Keep the synchronous relay, add duration only.** Doesn't deliver fan-out —
  the parent still blocks per child.

## References

- Issue #89: non-blocking sub-agent spawn (handle + `agent_poll`)
- [ADR-0022](0022-subagent-spawn.md): sub-agent spawn + synchronous relay (this supersedes the relay)
- [ADR-0023](0023-subagent-spawn-limits.md), [ADR-0024](0024-subagent-permission-gating.md): spawn limits + gating (still apply per launch)
- [ADR-0018](0018-turn-loop-stash-discipline.md): turn-loop stash discipline
