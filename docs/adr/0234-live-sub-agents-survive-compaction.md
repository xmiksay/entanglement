# 0234. Live sub-agents survive compaction

- Status: Accepted
- Date: 2026-10-14
- Issue: #862 (found in the #829 review)
- Amends: [ADR-0110](0110-compaction-successor-closes-predecessor.md) (the
  successor is no longer always a root, and the source's close no longer
  ends its live sub-agents), [ADR-0205](0205-every-compaction-forks-a-successor-session.md)
  (what survives the fork) and [ADR-0231](0231-compaction-successor-inherits-background-work.md)
  §4 (sub-agents are now moved too). Relates to
  [ADR-0161](0161-unified-async-work-background-flag-and-one-poll.md)
  (owner-scoped handles), [ADR-0162](0162-agent-send-supervising-a-sub-agent.md)
  (`agent_send`) and [ADR-0223](0223-non-compacting-session-fork.md)
  (`/fork` is unaffected).

## Context

A compaction fork sends `Spawn { parent: None, predecessor: Some(source) }`
and then `CloseSession { source }` (ADR-0205). `CloseSession` cascades over
the source's spawn sub-tree and tombstones every descendant (#180). So
compacting a session killed any sub-agent that was mid-task, and its answer
never arrived. ADR-0231 moved the source's jobs, scripts and retained outputs
to the successor but left sub-agents to the cascade.

There was a second gap. When a sub-agent compacted, its successor started as
an unregistered root. The parent's spawn-tree walk, its `poll` and its
`ListPending { root }` lost the child, and the parent's handle pointed at a
closed id.

## Decision

1. **Parent compacts: its live children move under the successor.** The
   supervisor does the move while it handles the successor's `Spawn`
   (`holly/supervisor/relink.rs`). For every direct child of the source with
   a running task it rewrites `parent_links`, updates the directory's
   `SessionInfo.parent`, sends the successor `ChildSpawned`, and emits
   `OutEvent::ParentChanged { session, parent }`.
2. **Sub-agent compacts: its successor takes its place.** A parentless
   successor `Spawn` inherits its predecessor's parent from `parent_links`, so
   it starts as a child of the same parent (`SessionStarted { parent:
   Some(p), predecessor: Some(source), root: false }`). Its starting mode is
   still the source's, because it is the same session going on. A `Spawn`
   that names a parent with a live successor joins that successor, even
   while the source is still registered (see §3).
3. **Ordering.** The fork sends `Spawn` and then `CloseSession` on the same
   channel from one task, and the supervisor handles frames one at a time. So
   the move has finished before the close is dequeued. When the cascade
   runs, the moved children are no longer in the source's sub-tree, so it
   ends only the source. That ordering covers the fork's own two frames
   only: another `Spawn { parent: source }` can arrive between them, for
   example from the runtime launching a sub-agent for a call the source made.
   Placement closes that gap: a parent with a live successor resolves to the
   successor whether or not the source is still registered, so such a child
   starts under the successor and the cascade never reaches it. `close`
   itself is unchanged.
4. **What moves.** Only live direct children move. A child that was closed
   earlier has already left `parent_links` and stays closed. A hibernated
   child has no task to keep, so the cascade retires it as before.
   Grandchildren come along with their parent.
5. **`/fork` moves nothing.** A user fork has no `predecessor` and its
   source stays alive (ADR-0223).
6. **The successor chain is shared.** The supervisor's private source →
   successor map moves into the shared `SessionDirectory`.
   `Holly::successor_of(id)` follows it to the newest link, and
   `Holly::lineage_head(id)` walks it back to the first. It is written
   before the successor's `SessionStarted` is broadcast, and a `Resume`
   re-records every lineage its log names, so both answers survive a
   restart.
7. **Runtime registry: alias, not re-key.** An `AgentRegistry` entry stays
   keyed by the handle the parent was given and gains `session`, the live
   session that answers for it.
   - `poll { handle }` keeps working without the model learning a new id.
   - `agent_send` prompts `session`.
   - Lifecycle events match on `session`, so the retired id's `SessionEnded`
     does not close the entry.
   - The subtree walk reaches the handle and walks on from `session`.
   - On a parent compaction, `parent` is rewritten.

   The registry is updated in two ways, following ADR-0231:
   - **Eagerly**, from the lifecycle broadcast: `SessionStarted { predecessor
     }` triggers `follow_successor`, and `ParentChanged` triggers `reparent`.
   - **Losslessly**, through `AgentRegistry::heal_from(&Holly)`. For each
     entry it follows `Holly::successor_of`, then reads the live session's
     parent from `Holly::session_info`. It runs before `poll`, `agent_send`,
     `ListOperations` and `ListPending`.

   The answer watcher (`collect_child_answer`) checks
   `Holly::successor_of` when it sees the child's `SessionEnded`. The
   successor's `SessionStarted` comes from a different task, so it can
   arrive after that end, or be lost to lag. Result delivery needs nothing
   else:
   - A background answer lands in the entry's watch channel, which ignores
     ownership.
   - A blocking `agent`/`agent_send` reply cannot meet a compacted parent: a
     session forks only between rounds with no outstanding tool call, and a
     blocking call keeps the parent parked until the reply lands. A result
     that did reach a compacted parent would be redirected to the successor
     and dropped there, since no call of the successor's is parked on it.
8. **Persistence follows the live tree through pointers, not by moving
   history.** The tap keeps logging a session to the root it started under
   for life, the `ParentChanged` included, so a session's records always sit
   in one file, in order. On `ParentChanged` it also appends a
   `LogPayload::Adopted { from }` pointer (record session = the moved child)
   to the new parent's root file. The pointer waits until that root resolves,
   because core announces the move before the successor's own
   `SessionStarted`, and is skipped when both are the same file.
   `SessionStore::read_tree` (used by `load_for_resume`) returns the root's
   records with each pointer resolved: the adopted sub-tree's records from
   the named file (itself read as a tree, so pointers chain), placed first.
   The pointer lives in memory until it is written, so a process that dies
   between the move and the successor's start loses it. `read_tree`
   therefore also reads the resumed root's immediate predecessor's tree
   (with its pointers resolved; the predecessor's id is its file's name) and
   adopts every session whose last `ParentChanged` there names a session of
   this tree and that no pointer already supplies. The resolved tree matters
   for a child moved more than once: its moves are logged in its own
   spawn-time root, which the predecessor reaches only by pointer. One hop is enough:
   only the newest compaction can be missing its pointer, since an older
   one's was written before the next compaction, and a lost `ParentChanged`
   leaves a `Gap` that refuses resume anyway. Each file is read once per
   call, and a pointer cycle in a damaged store terminates.
   Retention stays stat-only. When the tap writes a pointer it also touches
   the source file (`RecordSink::touch`, a no-op by default), restarting its
   retention clock; a live moved child keeps appending to it after that.
   Once it ages out regardless, resume drops the pointed sub-tree, as for a
   deleted file.
   Each session's records come from one file and a pointer always names an
   older file, so every session's history stays chronological, which is all
   replay needs. Replay folds `ParentChanged`, so the child comes back under
   the successor. After a cold resume the child logs into the successor's
   root (its re-announced `SessionStarted` names the new parent); the next
   resume still finds its earlier history through the pointer, ahead of
   those records. Only resume follows pointers: `skutter sessions`,
   `ReplayFrom`, `/fork` and file backups stay per file and never see them.
   A pointer whose file is gone (deleted, or aged out by retention) drops the
   adopted sub-tree, including any post-resume records it wrote into the
   adopter's file, instead of resuming it from a partial history.
9. **A sub-agent's successor logs into its parent's root file.** Before
   compaction forked sessions, a sub-agent always logged into its root's
   file; ADR-0110's root successor gave it a file of its own and lost it from
   the parent's tree. Starting it under its parent restores the original
   rule: it folds into the same root file, so resuming the parent's root
   restores it like any other sub-agent.
10. **Resumed sub-agents are tracked again.** A fresh process's
    `AgentRegistry` is empty. The lifecycle loop registers a sub-agent it has
    not seen when its `SessionStarted { parent }` arrives, under the handle
    the parent knew (`Holly::lineage_head`, the first id of its compaction
    lineage), and watches for its next answer. So `poll`, `agent_send` and
    `ListOperations` reach resumed sub-agents, however often they
    compacted. The event cannot tell a resume from a fresh `Spawn`, so a
    child an embedder spawns directly is tracked the same way: it becomes
    pollable by its parent, like a sub-agent the `agent` tool launched.
11. **Wire visibility.** `ParentChanged` is a seq-less lifecycle event that
   is persisted and folded on replay by overwrite. It is the only way a
   session's parent changes after its `SessionStarted`. The TUI re-nests the
   session, `SpawnGuard` re-links depth, and the hook tracker updates its
   payload. A compacting sub-agent needs no such event, because its
   successor's own `SessionStarted` names the parent.

## Consequences

- Compaction no longer interrupts delegated work. After the fork, the
  successor's handle-less `poll` lists its predecessor's running sub-agents,
  `poll { handle }` collects their answers, and `agent_send` reaches them.
  The predecessor's handle-less listing and subtree report follow the same
  links.
- `SessionInfo.parent` (and so `ListSessions`) can change during a session's
  life. Its other fields are still spawn-time facts.
- A root successor shares its predecessor's `SpawnGuard` running count, so
  the moved sub-agents keep counting against the tree's `max_agents`.
- Cost: a resume reads the resumed root's file, its immediate predecessor's
  file and the files its pointers name (recursively), each once. It grows
  with the moved sub-trees, not with the length of the compaction chain.
  `prune` reads no file.
- Retention: a pointed file can still age out once its moved sub-agents go
  quiet and outlive the retention window; resuming the adopter then omits
  that sub-tree rather than resuming it partially.
- Known limit, benign: a child that compacted while its eager fold was
  missed, and whose successor has itself closed since, keeps its last folded
  lifecycle, because `heal` cannot place a closed (or hibernated) session.
  This is the same staleness a dropped `SessionEnded` already causes for any
  child, and it fails closed: a tombstoned id is never respawned, so at worst
  `agent_send` is refused by the supervisor instead of by the registry.
- `/rewind` stays per root file (ADR-0212): file changes a moved child made
  are backed up under the root it started in, so rewinding the successor does
  not reach them.

## Alternatives considered

- **Skip re-parented children in `close`** instead of moving them at
  `Spawn`. `close` would need to know which children are about to move, and
  the tree would stay wrong between the two frames. Rejected: moving at
  `Spawn` keeps one source of truth.
- **Re-key the registry entry to the successor's id.** The parent's handle
  would stop resolving, and the model would need a new id it never saw.
  Rejected in favour of the alias.
- **Pass the parent in the fork's `Spawn` from the session's own `parent`
  field.** The session task's copy can be stale. The supervisor's
  `parent_links` is authoritative. Rejected.
- **Re-route the tap by the live root** after a move. The child's history
  would then be split across two files, so resume would have to merge them
  anyway. Moving the history with it would need the tap to re-read its sink,
  and `RecordSink` is append-only. Rejected.
- **Only walk the predecessor chain, with no pointers.** Correct, but a
  resume would read every file of the chain (100 chained files took 725 ms
  against 5 ms for one). Kept only as a one-hop fallback for a lost pointer.
- **Only pointers, with no chain walk.** A pointer is held in memory until
  the successor's root resolves, so a crash in that window would lose the
  child on resume. Rejected as the sole mechanism.
- **No wire event; heads re-query `ListSessions`.** No head polls it on a
  compaction, and the log would carry no record of the move to replay.
  Rejected.
