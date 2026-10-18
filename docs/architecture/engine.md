# entanglement Architecture — Per-session engine

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

This module is split into the pages below, one per seam; each stays under the 400-line doc cap (`make doc-cap`). Every section heading the single-file version had is kept here, pointing at where that section now lives.

| page | covers |
| --- | --- |
| [turn-loop](engine/turn-loop.md) | §5 — Turn loop, tool round-trip & live switching |
| [bounds-and-compaction](engine/bounds-and-compaction.md) | §5 — Re-offer, idle TTL, loop bounds & compaction |
| [session-ops](engine/session-ops.md) | §5 — Revert, fork, workdir, single-shot ops, ids, stop & pause |
| [subagents-and-plans](engine/subagents-and-plans.md) | §5 — Sub-agents, ask-user, plan acceptance & `rhai` |

## 5. Per-session engine (`session/`)

Now in [§5 — Turn loop, tool round-trip & live switching](engine/turn-loop.md#5-per-session-engine-session).
Continued in [§5 — Re-offer, idle TTL, loop bounds & compaction](engine/bounds-and-compaction.md).
Continued in [§5 — Revert, fork, workdir, single-shot ops, ids, stop & pause](engine/session-ops.md).
Continued in [§5 — Sub-agents, ask-user, plan acceptance & `rhai`](engine/subagents-and-plans.md).
