# 0231. A compaction successor inherits its predecessor's background work

- Status: Accepted
- Date: 2026-10-14
- Issue: #829 (review round 1). Re-parenting sub-agents is the separate issue #862.
- Amends: [ADR-0205](0205-every-compaction-forks-a-successor-session.md)
  (what survives the fork besides the seed); relates to
  [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md) §4/§6/§7
  (owner-scoped handles, the handle-less listing, retained outputs) and
  [ADR-0223](0223-non-compacting-session-fork.md) (`/fork` is unaffected)

## Context

Every compaction forks a successor (ADR-0205). The fork sends
`Spawn { parent: None, predecessor: Some(source) }` and then
`CloseSession { source }`. Closing tombstones the source and cascades over
its spawn sub-tree, so its sub-agents end. Its background `bash`/`call`
jobs, background `rhai` scripts and retained `o-` outputs do not end,
because nothing on the close path touches them. They are owned by the
source's id, though, and every owner check is an exact match
(`JobRegistry`, `ScriptRegistry`, `RetainedOutputRegistry`). So the
successor, which is the only session that continues the work, can neither
list nor poll them. Compaction is exactly when handles fall out of context,
so #829's handle-less `poll` recovery report would have recovered nothing
in the case it exists for.

## Decision

1. When the runtime's lifecycle loop sees `OutEvent::SessionStarted` with
   `predecessor: Some(p)`, it calls `reassign_owner(p, successor)` on the
   job, script and retained-output registries. Every entry owned by `p`
   becomes owned by the successor. Ownerless entries and other sessions'
   entries are left alone.
2. Only a compaction successor triggers this. `predecessor` is set only by
   the compaction fork. A `/fork` copy (ADR-0223) spawns with
   `predecessor: None`, because its source stays alive and keeps what it
   launched. `Spawn` is trusted-only (ADR-0069), so an untrusted wire frame
   cannot claim someone else's work by naming a predecessor.
3. Ordering. The successor's `poll` re-runs the move itself before reading
   anything (see Consequences), so its first `poll` sees the moved entries
   even if the eager fold was dropped. The order relative to the predecessor's
   `SessionEnded` does not matter, because ending a session does not kill,
   evict or cancel jobs, scripts or retained entries. A background launch
   never registers a canceller, and eviction is TTL-based.
4. Sub-agents are not moved. The close cascade still ends them (#862 tracks
   re-parenting them).

## Consequences

- After a compaction, the successor's handle-less `poll` lists the
  predecessor's still-running jobs and scripts. Its subtree section shows
  the retained outputs as "yours", and `poll { handle }` resolves them. The
  predecessor's id loses access, which is moot because it is closed.
- An owner is now mutable state, so `Job`/`ScriptOp` hold it behind a
  `Mutex`. The registries' public signatures are unchanged except for the
  added `reassign_owner`.
- The move runs in two places:
  - **Eagerly** in the lifecycle fold, which keeps a head's
    `ListOperations` right in the normal case.
  - **Losslessly** at the start of every `poll` by the successor, with or
    without a handle. The poll reads the successor's `predecessor` from the
    supervisor's directory (`Holly::session_info`), which is set at
    registration and restored from the log on resume, and re-runs the
    idempotent move. That covers a `SessionStarted` dropped by broadcast lag
    on the shared outbox. It also covers work the predecessor registers
    after the fold, in the window between the successor's `SessionStarted`
    and the close.
- Known limits:
  - The heal is one hop. In a chain A→B→C, suppose B's move from A was
    missed and B never polled before it was itself compacted. C heals only
    from B, so A's work stays under A.
  - A head's `ListOperations` can show the stale owner until the successor
    next polls, if the eager fold was dropped.
  - A resumed successor re-emits `SessionStarted`, which re-runs the move.
    That is harmless because the closed predecessor owns nothing new.

## Alternatives considered

- **Make the owner check lineage-aware** (allow a caller whose predecessor
  chain reaches the owner). Every registry would need the lineage map, and
  every check would walk it. It also keeps the dead id as the visible owner
  in listings. Rejected: one move at a single point is simpler.
- **Move on `SessionEnded` of the predecessor.** That event does not name
  the successor, so the runtime would have to remember the pairing from
  `SessionStarted` anyway. Rejected.
- **Move sub-agents too.** That requires undoing the close cascade for them,
  which is a larger lifecycle change. Deferred to #862.
