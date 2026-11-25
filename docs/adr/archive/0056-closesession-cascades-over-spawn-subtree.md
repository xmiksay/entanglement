# 0056. `CloseSession` cascades over the spawn sub-tree

- Status: Accepted
- Date: 2026-07-13

## Context

`InMsg::CloseSession { session }` retired only the *target* session: it dropped
that one command channel and tombstoned its id
([ADR-0028](0028-session-lifecycle-enumeration-and-backpressure.md)). The
supervisor tracks `parent_links` (child→parent, populated on `Spawn` #60), but
`CloseSession` never consulted it
([#180](https://github.com/xmiksay/entanglement/issues/180), part of the
engine-robustness epic #176).

Consequence: closing a session that had spawned sub-agents left every descendant
running. A sub-agent's answers flow back to its parent, so once the parent is
gone there is **no consumer** — yet the child keeps driving its turn loop and
burning provider tokens, invisibly, until the whole engine shuts down.

Leaving a child alive after a parent **`Stop`** is a deliberate choice for
async `agent`/`agent_poll` sub-agents (cancellation is out of scope,
[ADR-0026](0026-async-subagent-spawn-and-poll.md)). But `CloseSession` is the
*explicit destroy* path, and there was no cascade even there.

## Decision

On `CloseSession { session }`, close the target **and its whole transitive
spawn sub-tree** in one pass.

- A breadth-first `collect_subtree(root, parent_links)` walk gathers `root` plus
  every session whose parent chain leads back to it (the `parent_links` map is
  child→parent, so the walk expands children of each collected id). Session
  counts are small, so the O(n²) scan is a non-issue.
- Each collected id is retired exactly as a single close was: drop its command
  channel (`sessions.remove`) so the task's `rx.recv()` returns `None` and it
  emits `SessionEnded`, drop its `session_meta` + `parent_links` entries, and
  tombstone the id in `closed` (single-use, ADR-0028). Descendants that are not
  live are still tombstoned, matching the target's own liveness-independent
  tombstone.

This stays the explicit-destroy path only. A parent `Stop` still does **not**
cascade to un-polled children (ADR-0026 unchanged); this ADR narrows the gap for
the one path whose stated intent is "end this session (and its work)."

## Consequences

- Positive: no orphaned sub-agent survives a parent close, so the token-burn
  leak is closed — the load-bearing fix.
- Positive: every descendant emits its own `SessionEnded` and drops from the
  next `SessionList`, so a head sees the whole sub-tree collapse rather than
  ghost rows for children with a dead parent.
- Neutral: purely additive on the wire — no new message variant, just more
  `SessionEnded` events (which heads already render) for one `CloseSession`.
- Negative: closing a mid-tree node also closes its descendants, even a
  descendant a head might have wanted to keep. That matches the destroy intent;
  a head that wants to preserve a sub-agent closes the leaves it wants to keep
  first, or does not close their ancestor.

## Alternatives considered

- **Emit descendants as "orphaned" in `SessionList` instead of closing them.**
  The issue floated this as a floor. It surfaces the leak but does not stop the
  token burn — the actual harm — so it only informs a head that would still have
  to close each child itself. Cascading closes the leak at the source.
- **Add a reverse parent→children index.** A second map kept in lockstep would
  make the walk O(n) but doubles the bookkeeping the supervisor must keep
  consistent across `Spawn`/`CloseSession`/`Resume`. At realistic session counts
  the forward-map scan is free; a reverse index is premature.
- **Cascade parent `Stop` too.** Out of scope here and deliberately declined by
  ADR-0026 — `Stop` is cancel-a-turn, not destroy-a-session. This ADR touches
  only the destroy path.
