# 0246. A resumed sub-agent answers to its ancestors, and inherits its predecessor's teardown tail

- Status: Accepted
- Date: 2026-10-21
- Issues: #905, #906
- Amends: [ADR-0218](0218-agent-default-mode-is-a-starting-hint.md) (§5
  "a resumed session takes the mode its own log replays": a sub-agent now
  takes the stricter of that and its nearest ancestor's current mode),
  [ADR-0244](0244-seq-order-leases-and-one-task-per-id.md) (§4: the exit
  signal carries the old task's teardown tail; its "accepted limitation"
  bullet is resolved, see §4 below),
  [ADR-0210](0210-unknown-session-policy.md) (a load that finishes under a
  down ancestor is refused) and
  [ADR-0090](0090-idle-ttl-auto-hibernation.md) (a hibernated descendant
  counts as settled)

## Context

Operations that walk an ancestor's sub-tree only acted on *live*
descendants, so a hibernated or still-loading descendant missed them
(#905):

1. **Mode.** `SetMode` on a root reached a hibernated child only through
   `cascade_mode`'s cache write. A later resume folded the child's mode back
   from its own log, so it came back in its old, possibly wider mode under
   a `research` parent.
2. **In-flight load.** Under `UnknownSession::Resume`, a frame for child C
   starts a load. If the root was hibernated meanwhile, C was not a live task,
   so the hibernate skipped it, and the load then started C live under a
   hibernated root.
3. **Idle sweep.** `parent_links` keeps a hibernated child, but its activity
   entry is gone, so the sweep never saw the sub-tree as settled and the root
   was pinned live for good.

And ADR-0244 §4 accepted a gap (#906): a resume replays records the head
read before the old task finished its teardown, so whatever that task
emitted while tearing down (a late `FilesReverted`, say) reached the log
but not the new task's context. A later resume from the complete log then
diverged from the live state.

## Decision

1. **A resumed sub-agent's mode is clamped to its nearest ancestor's.**
   Every resume path goes through `Supervisor::spawn_resumed` (an explicit
   `Resume`, the resume cascade, an ADR-0210 load, a fresh process), which
   now decides the mode (`supervisor/ancestry.rs`, `resumed_mode`):
   - The **nearest ancestor** is the closest one in the parent chain: the
     supervisor's `parent_links`, else the log's `ParentChanged` /
     `SessionStarted`. An ancestor with a recorded compaction successor
     stands for that successor (ADR-0205), whether or not this process saw
     it close: the resume re-learns the lineages from the records'
     `SessionStarted { predecessor }` before `resumed_mode` walks
     (`Supervisor::learn_lineages`, ADR-0234), so the mapping does not
     depend on the tombstone set, which a fresh process has not filled.
     §2's `down_ancestor` walks before the resume and re-learns nothing: the
     ancestors it refuses for were live in this process, so their lineages
     are already known. The walk stops at the first live ancestor or the
     root.
   - Its **current mode** is its live one when it runs (the supervisor's
     `modes` cache). Otherwise it is its mode as the log records it (the
     whole tree folds into the root's file, ADR-0020), clamped the same way
     by its own ancestors, so it is the mode that ancestor would itself
     resume in.
   - The sub-agent takes the ancestor's mode when that is **stricter** and
     keeps its replayed one otherwise, on the one ordering core has:
     `EngineConfig::mode_ladder`, narrowest first, `research` < `plan` <
     `build` < `auto` for `skutter` (ADR-0218 §4;
     `entanglement_runtime::mode::BUILTIN_LADDER`). Equal modes are no
     change.
   - **An incomparable pair keeps the replayed mode** (a pair not both on
     the ladder: an embedder's own names, or the empty default ladder).
     This is deliberately *not* ADR-0218 §4's spawn rule, where an
     incomparable request falls back to the parent: a spawn's fallback is
     the parent's mode by construction, while a resumed session already
     holds its mode, and swapping it for one the ladder cannot show
     narrower could widen it — ADR-0216's forced `research` under a
     ladder-less engine is exactly that case. So a ladder-less engine
     resumes as before; `starting_mode::narrow_to` replaces a mode only by
     one provably no wider. The same holds while the old task still tears
     down (§4): on such an engine, or for off-ladder modes, the supervisor's
     cache cannot be shown stricter than the records, so a `SetMode` the
     old task took during its teardown is switched back to the records'
     mode by the start-up `ModeChanged`. That is consistent (the log
     records the switch back) and what master does; `skutter` always
     passes `BUILTIN_LADDER`, so it does not meet this.
   - A root has no ancestor and resumes in its replayed mode, as before.
   - **It is a real mode switch.** The decided mode reaches the task as
     `SessionStart::initial_mode` (no longer ignored on resume). When it
     differs from the replayed mode, `start_session` clears the mode-scoped
     tool overlay and sets the one-shot transition notice, as a live
     `SetMode` does, and the start-up `ModeChanged` (always emitted and
     logged) records it. A later replay folds that `ModeChanged` with the
     same overlay drop, so the log replays exactly. The runtime folds it as
     a switch too: in-process its `perm_modes` still holds the old mode, so
     the ADR-0243 lineage epoch moves and the overlay mirror is dropped; a
     fresh process has no prior mode and nothing to drop. That
     `ModeChanged` is what the runtime grades with and what its `mode_of`
     reads; `SessionInfo` carries no mode.
   - **The clamp is sticky.** It is applied once, when the sub-agent
     resumes, and recorded as a mode switch. An ancestor that later widens
     again does not widen the resumed child back: a `SetMode` cascades to
     live descendants with the new mode (ADR-0207 §6), and the resumed
     child, now live, follows that cascade like any live child, but nothing
     restores the wider mode it held before the clamp.
   - **`cascade_mode` only narrows a non-live descendant's cache entry**
     (`modes[d] = narrow_to(current, mode)`), where it used to overwrite it.
     The entry still matters: a child spawned under a hibernated parent
     seeds its mode from it, so it must not stay wider than an ancestor
     narrowed the tree to. And a resume reads it while the old task still
     tears down (the log may lag that task), so it must never be widened
     past the mode that task last ran in either. The cache replaces the
     log's mode only when provably stricter (narrow-only), so neither a
     stale log nor a head's narrowed replay (ADR-0216) is widened.
2. **A load that finishes under a down ancestor is refused.** Each
   `pending_loads` entry (`unknown::PendingLoad`) records every id a
   hibernate evicted while it was in flight. `finish_load` walks the loaded
   session's ancestors (from the loaded records) and, instead of starting
   it, refuses each queued message once (`refuse_queued`):
   - the loading id **itself was hibernated** while it loaded, by a
     `HibernateSession` naming it: `HIBERNATED_WHILE_LOADING`. Only the
     hibernate's own target marks the loading id this way. A hibernated
     child keeps its `parent_links` entry (#639), so a hibernate of its root
     reaches the loading child as a sub-tree member too; that counts as its
     ancestor going down and goes through the "still not live" check below,
     so a root resumed before the load finishes lets the child start;
   - an ancestor is **closed** (and not a compaction): `PARENT_CLOSED`;
   - an ancestor was **hibernated while the load ran** and is still not
     live: `PARENT_HIBERNATED`.

   A load that merely finds its ancestors not live (a fresh process, or a
   tree hibernated before the frame arrived) starts as an explicit `Resume`
   would, under the mode rule of §1. That is the case a lazy resume of a
   child exists for; only a hibernate that raced the load is the "parent
   went down under it" case. A root with no parent loads as before.
3. **The idle sweep counts a non-live descendant as settled.** A member of
   the sub-tree with no task (hibernated) is skipped; one with a load in
   flight pins the tree live, since it is about to run.
4. **The old task hands its teardown tail to the new one (#906).**
   - **Capture.** `hibernate_subtree` subscribes to the outbox for each
     victim *before* sending it `Hibernate`. The watcher that awaits the
     task (ADR-0244 §4) collects every event for that id until the task
     exits, then drains what is still buffered (`predecessor::collect_tail`).
     Live-only events (`OutEvent::is_live_only`: the nested-call rows) are
     left out, as persistence leaves them out of the log, so the tail
     mirrors what the persistence tap logged for the id from the
     `Hibernate` send to the exit, in log order.
   - **Hand-over.** The exit signal becomes `watch::Receiver<Option<Exited>>`
     (was `<bool>`): `Some(Exited { tail, last })` once the task exited.
     `last` is the old task's **final state**, its mode and tool overlay as
     it hibernated (`Final`). `session_loop` returns an `Ending` for its
     watcher: `Hibernated(Final)`, `Ended`, or `NeverRan(prior)` for a task
     closed or hibernated while it still waited on its own predecessor; such
     a task passes that predecessor's whole `Exited` on unchanged, so a task
     started behind it still gets the teardown tail and the final state. The
     task is the authority on mode and overlay, so the new task takes them
     as they are instead of rebuilding them from events.
   - **Fold.** A task started by `spawn_resumed` while the id is retiring
     also keeps the resume's records (`SessionStart::records`). After
     `predecessor::ready` returns the tail, `fold_tail` re-runs
     `Session::replay` over the records plus the tail's unseen suffix, so a
     head that read part of the tail already does not fold it twice: the
     suffix starts after the last tail event whose seq is at or below the
     records' highest seq for the id. Seq-less events right after that cut
     may already be in the records and are folded again, which is
     **idempotent**: a run of them folded twice leaves the state it left
     once — each sets fields to values the event carries, and `Status
     { Done | Paused }` and `SessionHibernated` close a turn that is already
     closed — with one exception, the overlay drop a `ModeChanged` makes,
     which depends on the mode before it. So the **final state replaces the
     folded mode and overlay**: a same-mode `ModeChanged` does not clear an
     overlay, so folding an overlay twice would revive one a switch
     dropped; and a head's appended ADR-0216 `ModeChanged` sits between the
     records and the tail, so the fold would apply a teardown overlay on top
     of the forced mode. `start_session` then applies the decided mode of §1 against the
     old task's own final mode: a difference is a switch and clears the
     overlay, exactly as a later replay of the log sees it (the old task's
     events, then the new task's start-up `ModeChanged`). The supervisor's
     own decisions on top of the replay carry over: the re-validated
     workdir and the lineage. For a retiring id
     the supervisor reads the newer word for mode and workdir from its
     caches (written by every `SetMode`/validated `SetWorkdir` routed to the
     old task), so it decides what the tail holds. For the mode the cache
     wins only when it is narrower than the records (narrow-only, as in §1);
     the workdir cache is re-validated like a replayed one. Then B's state
     is what a later replay of the log rebuilds.
   - **Bounded.** At most 1024 events (a teardown finishes one command and
     hibernates, so a real tail is a handful). Every event for the id
     counts, streaming deltas included: replay folds `TextDelta` into the
     assistant message and reads `ReasoningDelta`/`ToolCallDelta` as proof
     a round began streaming, so leaving them out would change the fold. A tail that overflows, or
     whose subscriber lagged, is dropped whole with a warning rather than
     handed over cut. A start that is not a resume (`Spawn`, a lazy blank
     root) ignores the tail.
   - **No tail and no final state when the old task panicked,** consistent
     with ADR-0244 §5: a panicked task writes no `SessionHibernated` and its
     last state is unknown, so the new task starts from the records alone.
     A lagged or overflowed tail still hands over the final state.
   - **What it does not cover.** Events for the id emitted *before* the
     `Hibernate` was sent but after the head read its records are the
     head's concern: it chose when to read its log. A `Prompt` the old task
     took up during its teardown is an inbound frame, paired into the log by
     the head's persistence; it is not in the tail (the turn it starts is
     cancelled at once by the hibernate's sender drop), so the resumed
     context lacks that prompt, while a later replay of the persisted log
     has it. A task that never
     ran passes its predecessor's tail and final state on, but not events
     other emitters authored for the id between that predecessor's exit and
     its own (it exits at once then). A task started after the old one has
     exited gets no hand-over at all: the records are then the head's
     complete log, its concern as above.
   - **A `Hibernate` stashed behind a mid-stream `Pause` hibernates.** Both
     are stashed while the round streams; the hold then waits on an inbox
     the hibernate already closed. The session loop now takes a stashed
     `Hibernate` when its inbox closes, so it hibernates (and hands over its
     final state) instead of ending. This predates the ADR; it matters here
     because an ended task hands over nothing.
   - ADR-0244 §4's resume-cascade skip of a still-retiring child stands
     unchanged: that child's tail ends in its own `SessionHibernated`, which
     prunes it from its parent's children, so the complete log would not
     revive it either.

## Consequences

- A sub-tree never resumes wider than an ancestor narrowed it to while it
  was hibernated, whichever path resumes it and whether or not the ancestor
  runs.
- The rule needs the ladder: an embedder without a `mode_ladder` resumes
  sub-agents in their replayed mode, as before. `skutter` always passes the
  built-in ladder.
- A resume behind a slow teardown sees that teardown's effects (a revert
  note, a mode or workdir switch) in the new task's context and state.
- One extra outbox subscriber per hibernated task, alive until that task
  exits; and a clone of the resume's records per task that has to wait for
  its predecessor.
- No public API change: `SessionStart`, `ExitSignal` and `PendingLoad` are
  crate-private.

## Alternatives considered

- **Persist the ancestor's `SetMode` on each hibernated descendant** (a
  `ModeChanged` written for it). Core does not own persistence, and a
  descendant with no task has nothing to emit through; the log would carry
  an event its own session never authored.
- **Take the ancestor's mode unconditionally.** It would widen a child the
  user explicitly narrowed below its parent.
- **Treat an incomparable pair as the spawn clamp does (ancestor wins).**
  It widened a head's forced read-only child on a ladder-less engine
  (`resume_config_mode`), a regression against today's behaviour.
- **Re-read the tail from the store before `start_session`** (#906's first
  direction). It needs a loader hook in core, which owns no persistence, and
  every head without one keeps the gap.
- **Hand over the old task's final `Context`.** It covers the context but
  not the rest of the session state (workdir, parked turn), and diverges
  from replay wherever live and replayed folds differ; the tail goes
  through the very fold a later replay runs. Only mode and overlay, which
  the fold demonstrably gets wrong (§4), travel as final state.
- **Snapshot the supervisor's mode cache at the `Hibernate` send** and
  clear the overlay on a mismatch. It closes the head-appended-mode case
  but still rebuilds the overlay from events; the task's own final overlay
  is exact.
- **Also skip a leading run of the suffix equal to the records' last
  events.** Tried: it can skip a genuinely new repetition (a deny and a
  same-mode switch re-sent during the teardown), and with mode and overlay
  taken from the final state every remaining seq-less refold is idempotent,
  so it bought nothing.
- **Buffer in the old task from the moment it pops `Hibernate`.** A task
  busy in a slow command (the revert of the example) pops it only after
  emitting the event that matters; the tail has to start at the send.
- **Refuse a load whenever an ancestor is not live.** It would break lazily
  resuming a child in a fresh process, where nothing is live yet.
