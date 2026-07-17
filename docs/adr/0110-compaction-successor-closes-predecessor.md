# 0110. `/compact` forks a *successor* and retires the source — and each session tracks its lineage

- Status: Accepted
- Date: 2026-07-17

## Context

[ADR-0101](0101-compaction-forks-into-a-new-session-copy-on-write.md) made
`/compact` copy-on-write: the source session's `Context` is never mutated, the
summary rides only in `OutEvent::Compacted`, and the TUI forks it into a **new
child session** via `InMsg::Spawn { parent: source }` while the source **stays
interactive** — its full pre-compaction history recoverable by resuming it (the
"implicit undo"). Two problems surfaced in use:

1. **The source lingering as a live, interactive session is confusing.** After a
   compaction the user means to *continue in the summarized session*, not keep two
   parallel conversations. The old source accumulates no further work but stays in
   the session list, promptable, indistinguishable from a real branch.
2. **Making the fork a `child` of the source is the wrong relationship.** A
   sub-agent child is spawned under a still-active parent and dies when the parent
   closes (the `CloseSession` cascade over the spawn sub-tree,
   [ADR-0077](0077-session-hibernation-evictable-resumable.md)). A compacted
   session is not a sub-agent of its source — it *replaces* it.

Separately, the engine tracked lineage only one way: `Session.parent:
Option<SessionId>` existed, but there was no `children` mirror on the session, and
no notion of a "predecessor/successor" relation distinct from parent/child.

## Decision

**`/compact` forks a *successor* — a fresh root session that records its
predecessor for lineage but joins no spawn sub-tree — and closes the source's
interactive session right after. Add explicit two-way lineage (`children`,
`predecessor`) to `Session` and the protocol.**

- **`InMsg::Spawn.parent` becomes `Option<SessionId>`** and gains
  `predecessor: Option<SessionId>`. `parent = Some(p)` is the ordinary child
  spawn (unchanged semantics). `parent = None` is a **root** spawn — used only by
  the compaction fork — carrying `predecessor = Some(source)`: lineage, never a
  spawn edge, so the source is *not* an ancestor and closing it can't cascade onto
  the successor. `OutEvent::SessionStarted` carries `predecessor` too (both
  `#[serde(default, skip_serializing_if)]` for log back-compat).
- **The TUI compaction handoff** (`tui::app::compact`) spawns the successor with
  `spawn_for_fork` (`parent: None, predecessor: Some(source)`) and, once it's
  sent, closes the source with `close_predecessor` → `InMsg::CloseSession`. The
  user moves forward into the compacted successor; the original's interactive
  session is retired (tombstoned, resume refused per ADR-0077) while its
  `{root}.jsonl` **log is preserved**.
- **`Session` gains `children: Vec<SessionId>` and `predecessor: Option<SessionId>`.**
  `children` is populated live via two new internal `SessionCmd::ChildSpawned`/
  `ChildClosed` the supervisor sends the parent task on `Spawn` (mirroring the
  `parent_links` edge it records) and on the `CloseSession` cascade (to the
  still-live parent of the closed sub-tree root). Both are pure state updates,
  applied immediately even mid-turn, and idempotent. `predecessor` is set from the
  `SessionStarted` the session emits.
- **Replay reconstructs both** without new log fields: `children` by inverting the
  parent edges already in the shared root log (a child's `SessionStarted {
  parent: root }` adds it, its `SessionEnded`/`SessionHibernated` removes it — the
  same inversion `collect_subtree`/`session_store::children_of` already do), and
  `predecessor` from the resumed session's own `SessionStarted`.
- **Auto-compaction is unchanged.** The in-place, mid-turn overflow path
  ([ADR-0103](0103-auto-summarize-on-context-overflow.md), `auto: true`) mutates
  the live `Context` and continues the same turn — it has no head to fork into and
  no source to retire. Only the manual `/compact` path (`auto: false`) forks a
  successor and closes the source.

## Consequences

- **Positive.** `/compact` now behaves like a "move forward": one session, the
  compacted successor, active; the bloated predecessor retired. The session list
  reads as a lineage (`predecessor` links) rather than an ever-growing set of
  parallel forks.
- **Positive.** The successor being a root, not a child, makes closing the source
  safe: the `CloseSession` cascade walks the spawn sub-tree, which the successor —
  linked only by `predecessor` — is not part of. The permission ancestor clamp
  ([ADR-0024]) likewise never treats the source as the successor's ancestor.
- **Positive.** Two-way lineage is now first-class on `Session` (`parent` +
  `children` + `predecessor`), reconstructed losslessly on replay from edges the
  log already carried — no redundant serialized `children`/`successor` field to
  drift from the authoritative `parent`/`predecessor` edges.
- **Negative / accepted — this reverses ADR-0101's "implicit undo".** Closing the
  source means it can no longer be resumed (closed ids are single-use, ADR-0077),
  so the pre-compaction history is no longer recoverable by re-entering the source
  — it survives only as the persisted `{root}.jsonl` log for audit/replay by an
  embedder, not as an interactive session. This is the deliberate product change
  the user asked for; ADR-0101's copy-on-write *at the engine level* still holds
  (the engine never mutates the source's `Context`), it's the head that now
  retires it.
- **Neutral.** `Session.children` is a per-session **mirror** of the supervisor's
  authoritative `parent_links`; it exists for the engine/embedder's own use, and
  the supervisor still derives sub-tree membership from `parent_links` (not the
  in-task `children`). The two are kept consistent by the `ChildSpawned`/
  `ChildClosed` commands and replay inversion.

## Alternatives considered

- **Keep the fork a child of the source and just close the source.** Rejected:
  the `CloseSession` cascade closes the whole spawn sub-tree, so closing a source
  whose child is the successor would kill the successor too. Decoupling the
  successor from the source's sub-tree (root + `predecessor`) is what makes
  "retire the source" safe — hence the `parent: Option` change.
- **Add a `successor: Option<SessionId>` field alongside `predecessor`.**
  Rejected: the forward direction is the inverse of `predecessor` and derivable
  the same way `children` inverts `parent`; a stored `successor` would be
  redundant state that can drift. One direction of the edge is enough.
- **Serialize `children` explicitly in `SessionStarted`/a new event.** Rejected:
  `children` is fully reconstructible from the `parent` edges the log already
  records; a redundant serialized field risks diverging from the authoritative
  parent links (the same reasoning [ADR-0106] used to avoid a redundant
  `skill_id` protocol field).
- **Leave the source interactive (ADR-0101 unchanged), only relabel the UI.**
  Rejected: the user explicitly wants the original's interaction *closed* on
  compaction, not merely styled differently — a live, promptable source is the
  confusion being removed.

[ADR-0024]: 0024-ancestor-permission-clamp.md
[ADR-0106]: 0106-skill-scoped-allowed-tools-enforcement.md
