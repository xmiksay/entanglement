# 0112. `Resume` cascades over the spawn sub-tree; the re-announced `SessionStarted` carries the replayed `predecessor`

- Status: Accepted
- Date: 2026-07-17
- Issue #415, reported alongside the sub-agent spawn work
  ([PR #413](https://github.com/xmiksay/entanglement/pull/413)). Builds on the
  two-way lineage (`children`/`predecessor`) [ADR-0110](0110-compaction-successor-closes-predecessor.md)
  added to `Session`, and mirrors the sub-tree cascade
  [ADR-0056](0056-closesession-cascades-over-spawn-subtree.md) (`CloseSession`)
  and [ADR-0077](0077-session-hibernation-evictable-resumable.md)
  (`HibernateSession`) already use on teardown.

## Context

`Holly::resume(root_id, records)` rebuilt exactly **one** session:
`Session::replay` correctly reconstructed that session's `children` (by
inverting the `parent` edges in the shared root log, per ADR-0110) and
`predecessor` (from its own `SessionStarted`) as *data*, but the supervisor's
`InMsg::Resume` handler never acted on the reconstructed `children` — it
registered only the target id in `sessions`/`session_meta`/`parent_links` and
spawned a single `session_loop` task.

Consequence: a session that had spawned sub-agents (or was itself a
grand/child) came back from a resume as a leaf with a *cosmetically* correct
`children: Vec<SessionId>` field but no live task behind any of those ids.
Touching a child id afterward — a `Prompt`, a `CloseSession` — fell through to
the supervisor's lazy-respawn path (`holly.rs`'s unmatched-id branch), which
creates a **blank** session under that id: the child's task-instruction context
and turn history are silently discarded, and the resurrected id shows up
rootless (`parent: None`) because the `parent_links` edge hibernation/restart
tore down was never re-established. `CloseSession`/`HibernateSession` already
solved the mirror-image problem (tearing down a whole sub-tree on teardown);
`Resume` had no equivalent cascade rebuilding one.

A second, narrower bug surfaced while fixing the first: `session_loop`
re-announces `SessionStarted` on every start, including a resume, but built the
*announced* event from the raw `predecessor` parameter — which `Holly`'s
`Resume` handling always passes as `None` so it can't clobber the value
`Session::replay` already reconstructed onto the in-memory `Session`. That
`predecessor: None` event was itself persisted, so replaying the log a
*second* time folded it last (per-`SessionStarted` last-write in
`Session::replay`) and lost the predecessor for good — a resumed compaction
successor ([ADR-0110](0110-compaction-successor-closes-predecessor.md)) that
gets hibernated/resumed again silently forgets its source.

## Decision

**`Resume` cascades exactly like `CloseSession`/`HibernateSession`, in
reverse: after replaying and spawning the requested session, walk its
replay-reconstructed `children` and recursively replay-and-spawn each one that
is still "live" in the log (a `SessionStarted` with no matching
`SessionEnded`/`SessionHibernated`), re-registering `parent_links` as it goes.**

- `Session::replay(records, cfg, target: &SessionId)` gains an explicit
  `target` parameter. Previously it auto-detected "the root" as the first
  `SessionStarted { root: true, .. }` in the log and only ever reconstructed
  that one session (the sole production caller always resumed a root id, so
  this coincided). Generalizing to an explicit target — with the same
  `is_target`-scoped fold that already kept a spawned child's interleaved
  records from bleeding into the wrong session's `Context` (#275) — lets the
  identical function reconstruct *any* session in a shared root log, root or
  descendant. `target`'s own `children` (direct only; grandchildren belong to
  their own parent) and `parent`/`predecessor` come out of the same fold as
  before, just scoped to whichever id is asked for.
- A new supervisor helper, `spawn_resumed`, factors out "replay one session
  from the shared log and register it exactly like a live `Spawn` would"
  (`session_meta`, `parent_links` when it has a parent, a spawned
  `session_loop` task, the `sessions` map entry) — the single per-node
  operation both the initial resume and the cascade share. It returns the
  node's replayed `children` so the caller keeps walking.
- The `InMsg::Resume` handler: `spawn_resumed` the requested id, then BFS the
  returned `children` (extending the queue with each node's own `children` as
  they're spawned) — a plain `Vec` + cursor, matching `collect_subtree`'s
  existing iterative style rather than recursion. A child already closed
  (tombstoned) or already live is skipped, defensively mirroring the guards
  the single-session path already had.
- `resume_meta` (the `SessionInfo` derived from a replay log, feeding
  `ListSessions`) had a latent bug this cascade exposed: it returned the
  **first** `SessionStarted` record in the whole log, unscoped by session —
  harmless when every call targeted the log's own root (whose first record
  *is* its own `SessionStarted`), wrong the moment the same log is asked for a
  non-root target's meta. Fixed to match on `session == target`.
- `session_loop`'s re-announced `SessionStarted` now emits the *resolved*
  predecessor — `s.predecessor` (set from replay when present, else the raw
  `predecessor` parameter for a fresh spawn) — instead of the raw parameter
  alone. The in-memory `Session.predecessor` and the wire event it announces
  can no longer disagree, so a second resume of the same log no longer
  regresses the lineage.

No wire/protocol shape changed: no new `InMsg`/`OutEvent` variant, no new
`SessionInfo` field. The fix is entirely in how the supervisor and
`Session::replay` use data the log already carried.

## Consequences

- **Positive.** A crash/restart (or an explicit `HibernateSession` cascade)
  followed by resuming the root now brings the *whole* previously-live spawn
  sub-tree back, not just the one named id — symmetric with how
  `CloseSession`/`HibernateSession` tear down the whole sub-tree in one call.
  A parent can reach and continue its sub-agents again.
- **Positive.** `Session::replay` becomes reusable for reconstructing any
  session in a shared log, not just its root — the seam a future feature
  needing per-node replay (e.g. resuming a child directly, if that ever gets
  its own entry point) can reuse without another generalization pass.
- **Positive.** The `predecessor` re-announcement fix makes hibernate→resume
  idempotent for lineage: resuming the same session any number of times keeps
  reporting the same predecessor, matching how `children`/`parent` already
  behaved.
- **Neutral.** A child resumed via the cascade is registered with the same
  `parent_links`/`session_meta` bookkeeping a live `Spawn` produces, so
  `CloseSession`/`HibernateSession` issued against the resumed parent
  afterward cascade over the resumed children exactly as they would over
  freshly spawned ones — no special-casing needed on the teardown side.
- **Negative / accepted.** A malformed or partial replay of one *descendant*
  (a corrupt interior record scoped to that child) only drops that branch —
  logged and skipped — rather than failing the whole resume; the root and
  every sibling branch still come back. This mirrors `list_sessions`'
  per-file skip-and-warn posture (#104) rather than the top-level `Resume`
  refusal (which still fails the whole request if the *target itself* fails to
  replay, unchanged).
- **Non-goal.** The originating prompt text for a spawned child is still not
  reconstructed on replay — `InMsg::Spawn`'s initial task is delivered directly
  to the child's session-command channel, bypassing the inbound broadcast the
  persistence tap observes, so no `InMsg::Prompt` record exists for it in the
  log (only the assistant's reply is folded, via the `Done` flush). This
  predates and is orthogonal to the cascade fix here — filed as
  [issue #421](https://github.com/xmiksay/entanglement/issues/421) and tracked
  in [`../deferred-work-ledger.md`](../deferred-work-ledger.md) rather than
  folded into this change.

## Alternatives considered

- **Only fix the `predecessor` re-announcement bug; document the child-cascade
  gap as accepted (root-only resume).** Rejected: the bug report's primary
  complaint — "the spawned sub-agent sessions are gone" — is exactly the
  child-cascade gap, and `CloseSession`/`HibernateSession` already establish
  cascading-over-the-sub-tree as this engine's norm for any operation that acts
  on a session's lineage. Leaving `Resume` as the sole non-cascading lifecycle
  operation would be a needless asymmetry.
- **Have the resuming head explicitly `Spawn`/`resume` each child it cares
  about, driven by the reconstructed `children` list on the target's own
  `Session`.** Rejected: `children` is supervisor/session-internal state, not
  exposed on the wire (`SessionInfo` carries only `parent`, per ADR-0110's
  "no redundant field" reasoning) — a head would need a new query just to
  learn what to resume, and would still have to reason about tombstoned vs.
  live descendants itself. Cascading once, in the one place that already holds
  the full log, does the same reasoning a single time instead of once per
  head.
- **Serialize `children`/a full sub-tree manifest into `SessionStarted` so
  resume doesn't need to re-scan the log.** Rejected for the same reason
  ADR-0110 rejected a stored `successor` field: it's redundant, derivable data
  that would drift from the authoritative `parent` edges the log already
  carries; the O(n) scan `Session::replay` already does per session is not a
  bottleneck at realistic session-tree sizes.
