# 0026. Non-blocking sub-agent spawn with handle + `agent_poll`

- Status: Accepted
- Date: 2026-07-09

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

## Resolved questions

- **Handle-table location & lifetime.** A shared `AgentRegistry`
  (`Arc<Mutex<HashMap<SessionId, Entry>>>` in `runtime::agent_poll`) cloned into
  each detached launch-watcher and `agent_poll` task — *not* the executor's
  single-threaded loop, since both writers and readers are separate tasks. Each
  entry holds the launch `Instant` plus a `watch::Receiver<AgentStatus>`; the
  watcher owns the `Sender` and flips it to `Complete { answer, elapsed }` on the
  child's `Done`. The mutex is only held to insert or clone a receiver, never
  across an `.await`. Entries **persist for the executor's lifetime** (bounded by
  `MAX_SPAWNS_PER_ROOT` = 16 per root), so re-polling a finished child is
  idempotent — no drain-on-read. Because the registry keeps a receiver, a
  completed `watch` value survives the watcher dropping its sender, so a late
  poll still reads the answer.
- **`agent_poll` on an unknown `agent_id`** → a clear **error** `ToolOutput`
  ("no sub-agent found for agent_id …"), not an empty result. A missing/empty
  `agent_id` argument gets its own guidance message.
- **Guard/permission interaction.** `SpawnGuard` budgets (ADR-0023) and ancestor
  permission clamping (ADR-0024) are charged/enforced **only at launch**, exactly
  as before. `agent_poll` starts no session and touches no host resource — it
  only reads accumulated state — so it is intercepted before permission
  resolution and needs no gating or budget charge.
- **`Stop` on the parent** does **not** cascade to cancel outstanding un-polled
  children — a launched child runs to completion regardless (the documented
  "runs unobserved" trade-off below). This matches the prior behavior (a `Stop`
  while parked on the synchronous relay already left the child running) and keeps
  cross-session cancellation out of scope; a `Stop`-cascade is deferred.
- **TUI affordance.** The sessions list (`tui::modals::draw_sessions_modal`)
  shows each sub-agent's (depth > 0) spawn duration next to its state — a live
  `⏱ Ns` while running, a fixed `✓ Ns` once ended — computed by `SessionView`
  from the `SessionStarted`/`SessionEnded` `ts` against the current wall clock.
  This is independent of the runtime registry (a head only sees `OutEvent`s).

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
