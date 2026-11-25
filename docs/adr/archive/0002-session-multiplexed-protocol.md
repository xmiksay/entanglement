# 0002. Session-multiplexed wire protocol

- Status: Accepted
- Date: 2026-07-04

## Context

The web/serve head must drive many independent conversations over a **single**
WebSocket (the `agent` reference model: one socket per browser, frames routed by
`task_id`). Editors and the TUI also benefit from separating conversations by id.

A separate concern: when a client reconnects and replays persisted history, it
needs to dedupe/order against live frames. `agent` solves this with a monotonic
per-task `seq`.

## Decision

Every `InMsg` and `OutEvent` carries a `SessionId` (a serde-transparent string
newtype). One transport connection multiplexes all sessions, routed by
`SessionId`; the engine lazily spawns one tokio task per `SessionId`.

**Content events** (`Plan`, `TextDelta`, `ToolRequest`, `ToolOutput`, `TaskList`,
`Error`, `Done`) carry a monotonic per-session `seq` for dedup/ordering.
**Lifecycle frames** (`Status`, `AgentChanged`) are point-in-time and carry no
`seq` (they are not part of replayable history).

This is chosen **from day 1**, even though only stdio uses it now, because
retrofitting `SessionId` + `seq` onto a single-session protocol later reshapes
every message shape.

## Consequences

- **(+)** Web-ready from the start; the stdio head is just a degenerate
  single-session case.
- **(+)** `seq` gives free dedup/ordering for future history replay.
- **(−)** Per-session `seq` bookkeeping inside the engine.

## Alternatives considered

- **One connection per session.** Rejected: web clients want one socket
  multiplexing every task; this is the proven `agent` model.
- **A single global session now, add multiplexing later.** Rejected: the message
  shapes would all change in the later phase — paying the cost twice. The cost
  now is small (an id field + a lazy-spawn map).
- **`seq` on every frame including lifecycle.** Rejected: `Status`/`AgentChanged`
  are point-in-time; giving them `seq` implies they're replayable, which they're
  not.
