# 0244. Seqs in delivery order, leased by detached work, one task per id

- Status: Accepted
- Date: 2026-10-20
- Issues: #891, #892, #893
- Amends: [ADR-0239](0239-retired-session-keeps-a-seq-high-water.md) (§1–§3:
  the registry keeps the counter itself, not its value; §6: the cap bounds
  only unpinned entries; §7: replaced by §4 below; its Consequences naming
  #891/#892/#893 and its rejected "lease" alternative) and
  [ADR-0068](0068-shared-per-session-seq-counter.md) (`Session.seq` is an
  `Arc<SeqCell>`, and a seq is minted and broadcast in one critical
  section)

## Context

ADR-0239 left three gaps in the seq contract.

- **#891.** The retired map was capped at 4096 entries and evicted the least
  recently touched one. A background script waiting on long work doesn't
  touch its entry, so an embedder retiring more than 4096 sessions in that
  time evicted it. The script's next card was minted with seq `0`, a
  strict-dedupe head dropped it, and the attended park waited forever: the
  #884 bug again.
- **#892.** `hibernate_subtree` sent `Hibernate` without waiting for the
  task to exit. A `Resume` (explicit, lazy, or `UnknownSession::Resume`)
  could start task B for the id while task A was still tearing down. Both
  could emit and touch the log and outbox for one id. A's `commands::retire`
  also removed the `ActivityRegistry` entry B had already written, so B
  counted as not idle until its next idle point.
- **#893.** `emit_for_session` minted the seq under the registry lock but
  broadcast after releasing it, and the session task minted from its atomic
  and sent separately. Two emitters for one session (the session task and a
  runtime service, or two runtime tasks) could deliver seq 6 before seq 5,
  and a `seq > last_seen` dedupe (the TUI's) dropped 5. This predates #884.

## Decision

1. **One `SeqCell` per session, minted and sent under its lock (#893).**
   `holly/seq_cell.rs`: `SeqCell` wraps a `Mutex<u64>`. `SeqCell::emit`
   (session task) and `with_next` (registry) mint the next seq and run the
   send inside the critical section, so broadcast order is seq order for
   every emitter of the session. Every path goes through it: the session
   task's emits (`session/emit.rs` and the inline ones), `emit_for_session`,
   and the supervisor's `Error` (`emit_supervisor_error`). The lock is per
   session, so sessions never wait on each other; the broadcast `send` never
   blocks. `emit_for_session`'s `make` runs under the lock, so it must not
   block or call back into `Holly` at all (no `emit_for_session` for any
   session, no `seq_lease`, no `is_live`): each takes the registry lock,
   which `register` holds while it locks a cell, so a callback could
   deadlock against a concurrent session start. An emit holds the registry
   `Mutex` only to look the cell up (an `Arc` clone) and releases it before
   locking the cell. The registry lock may be held while a cell is locked
   (`register` raises the cell under it), never the reverse: no path takes
   the registry lock from inside a cell's critical section, so the two locks
   are always taken in one order.
2. **The registry keeps the cell, not a value.** `retire` moves the cell from
   `live` to `retired`. `register(id, seed)` hands the id's retired cell back
   (raised to the replay's highest seq), else a fresh one, and the session
   task emits through what it returns (`s.seq = seqs.register(..)`). One id
   has one cell for as long as the registry remembers it, so a stale clone
   (an emit in flight, a lease) always mints in the same sequence as the
   task. ADR-0239's `fetch_max` seeding becomes `raise`.
3. **A lease pins a counter (#891).** `Holly::seq_lease(&id) -> SeqLease`
   clones the id's cell. Eviction skips any retired entry whose cell is held
   outside the registry (`Arc::strong_count > 1`): a lease, an emit in
   flight, or a task that retired but hasn't dropped its `Session` yet. The
   cap bounds the unpinned entries only. **When every entry over the cap is
   pinned, the map grows past the cap**; it never falls back to seq `0` for
   a pinned id. The growth is bounded by the number of live leases, and the
   overshoot is reclaimed at a later touch once the holders let go. A lease
   of an id that never started pins nothing and creates no entry (ADR-0239
   §5 still holds). The lease is RAII: dropping it is the release.
   - **The runtime takes it for detached work.** `script::background::
     run_background` takes a lease on the script's session before it replies
     with the handle (the launching turn is parked on that very call, so the
     session is usually live; a lease taken after it retired, say behind a
     hibernate, pins the retired cell just the same). It lives in
     `Dispatcher::seq_leases` and drops with the dispatcher when the script
     ends. The same holds for a successor that retires before the follow
     takes its lease.
   - **It follows compaction by covering the lineage.** When a detached run
     switches to a successor (ADR-0241), `follow_session` adds a lease on
     the successor and keeps the predecessor's. This is defensive: the
     binding loop is serial and switches only between calls, so no call
     parked as P is outstanding at the switch, and nothing in `skutter`
     emits as P afterwards. Keeping P's pin costs one `Arc` and holds if a
     later change lets work emit as P after the switch.
   - **It spans a resume.** The lease holds the cell, and a resume hands that
     same cell to the new task (§2), so a lease taken before a hibernate
     pins the counter the resumed task emits through.
4. **One task per id: the new task waits, not the supervisor (#892).**
   The supervisor keeps each task's `JoinHandle` (`tasks`, beside
   `sessions`). `hibernate_subtree` hands each hibernated task's handle to a
   watcher and records the id's exit signal (a `watch` channel, `true` once
   the task has exited, also after a panic, which the watcher logs) in
   `retiring`. `retiring` holds no messages. Every start path — `Spawn`,
   an explicit `Resume`, a resume cascade, an `UnknownSession::Resume` load
   completing, a lazy `SpawnBlank` start — proceeds at once: it inserts the
   new task into `sessions` and `tasks` as on master and hands it the exit
   signal (`SessionStart::prior`, taken out of `retiring`). The new task's
   first step (`session::predecessor::ready`) awaits that signal, before it
   registers its seq cell, announces itself or emits anything. Until then
   commands buffer in its own inbox in arrival order, exactly as for a live
   session busy in an await that doesn't watch its inbox. So to routing, to
   ancestor cascades (`hibernate_subtree`, `cascade_mode`), to `close`, to
   `adopt_children` and to the idle sweep the waiting task is an ordinary
   live session, and a hibernate, `SetMode`, `Stop` or prompt sent to it is
   simply handled once it starts, in order. There is no supervisor-side
   replay, so nothing is re-cascaded, double-refused or re-admitted.
   - **The seq cell.** The new task registers only after the old one has
     retired, so `register` hands it the old task's cell (§2), raised to the
     replay's seq; nothing is minted for the id from the new task before
     that.
   - **Closed or hibernated before it ran.** If the waiting task's inbox is
     closed when the old task exits (its id was closed, or hibernated
     again), it never starts and ends without emitting; what was sent to it
     is dropped with it. A close of a waiting task is a close of a
     hibernated id: no `SessionEnded`, and its buffered frames get no reply.
   - **`Holly::is_live`** reads the seq registry, so it is `false` from the
     old task's retire until the waiting task registers, which can span the
     whole teardown, although the supervisor already routes to the waiting
     task and lists it. A hibernate of a waiting task makes it retiring in
     turn: the next task under the id waits on its exit signal, which fires
     only after the old one's. The watcher drops an id's `retiring` entry
     on exit unless a newer task's signal replaced it (`same_channel`).
   - **A close is never blocked.** `CloseSession` tombstones the id at once
     as on master; a stuck old task only delays its own late
     `SessionHibernated`.
   - **The supervisor never waits**, and its sends into a waiting task's
     inbox behave as for any live session: routed messages use
     `route_to_session` (`try_send`, then shed with an `Error` when the
     inbox is full); the few lifecycle commands (`Hibernate`,
     `ChildSpawned`/`ChildClosed`) use a blocking `send` into a 64-slot
     inbox, which can wait only if 64 commands are already buffered — the
     same exposure as a live session stuck in a non-preemptible await today.
   - **A resume cascade skips a still-retiring child.** Unless its task
     panicked, such a child's `SessionHibernated` is still to come, so the
     records the cascade sees lack only that tail; the complete log (what the
     persistence tap holds once the child exits, and what a fresh process
     resumes from) prunes the child from its parent's children, so the
     normal path never revives it either. A child an explicit `Resume`
     already started (waiting) is live and skipped as usual. (A panicked
     task writes no `SessionHibernated`, so its complete log would revive
     the child; that edge is accepted.)
   - With the old task's exit ordered before the new task's start, the old
     task's retire can no longer drop the new one's seq counter or
     `ActivityRegistry` entry.
   - **Accepted limitation: a resumed task's records can miss the old
     task's teardown tail.** The records a resume replays were read before
     the old task finished, so what it emitted while tearing down (a late
     `FilesReverted`, say) can reach the log but not the new task's
     context. This is the same staleness the cascade skip above relies on;
     it predates this ADR, and on master the overlap made it worse
     (#906).
5. **ADR-0239 §7 is simplified away.** `retire(session)` no longer takes the
   task's own counter or compares `Arc::ptr_eq`, and `register` no longer
   raises over a live entry it replaces. A task that exits normally retires
   before §4 lets the next task register, so there is no live entry to
   replace. A task that panicked never retires and does leave one: the next
   task then shares that cell (§2), so its seqs continue above everything
   minted meanwhile and never repeat.

## Consequences

- A head's `seq > last_seen` dedupe folds every event, whichever task
  emitted it. Heads need no change.
- `SessionHibernated` always reaches the outbox before the resumed
  `SessionStarted`, and nothing the old task authors follows it. A resume
  of a just-hibernated id is delayed by the old task's teardown, usually
  microseconds; a session inside a slow non-preemptible await delays only
  its own new task, never the supervisor.
- A background script's card gets a real seq however many sessions retire
  while it runs, and after compactions too. ADR-0239's "very long-lived
  engine" limit is gone for leased work. Unleased late emitters (none in
  `skutter`) keep the cap's behaviour.
- **API changes for embedders.** `Session.seq` is `Arc<SeqCell>` (was
  `Arc<AtomicU64>`; read it with `.current()`). New `Holly::seq_lease` and
  `SeqLease`, `SeqCell` re-exported at the crate root. Migration notes in
  [migrations](../embedding/migrations.md).
- The session task's per-event cost is a `Mutex` lock instead of an atomic
  add. The lock is uncontended except while a runtime emit for the same
  session is in progress.

## Alternatives considered

- **An `EngineConfig` knob for the cap.** It moves the cliff without
  removing it; any finite cap fails for long enough idle work.
- **Fall back to seq `0` when every entry is pinned.** That is the #884
  failure for exactly the work the lease exists to protect. Growing past the
  cap is bounded by the live leases.
- **An explicit pin counter in the registry.** It needs a `Drop` that takes
  the registry lock and a count kept in step with the holders. The cell's
  strong count already is that count, and it also pins an emit in flight,
  which closes the eviction-versus-emit race.
- **Broadcast under the registry lock.** It serializes runtime emits of
  every session, and the session task would have to take the global lock on
  every `TextDelta`. Per-session cells keep the critical section per
  session.
- **Heads dedupe by a seen-set instead of a high-water.** It fixes only the
  heads that change, and an out-of-order delivery is still a contract
  violation for every other consumer (persistence taps, replays).
- **Await the old task inline in the supervisor.** Simpler, but a teardown
  stuck in an await that doesn't watch the inbox (a file revert, a
  summarize) would stall routing for every session.
- **Queue the id's messages in the supervisor until the old task exits,
  then replay them.** Tried first. Every cascade and refusal then needs a
  queue-aware twin: a replayed cascade `SetMode` re-cascaded over a
  grandchild against a later narrowing, injected cascade entries were
  refused as if a head had sent them, and a replayed `SetMode` for a
  non-live id went through the unknown-session policy again. Letting the
  new task wait keeps every supervisor path as on master.
- **Lease only the current session when a script follows a compaction.**
  Enough for today's serial binding loop, which emits nothing as the
  predecessor after the switch. Covering the lineage costs one `Arc` per hop
  and keeps a future emitter on the predecessor safe.
