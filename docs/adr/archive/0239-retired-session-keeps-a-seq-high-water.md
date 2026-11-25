# 0239. A retired session keeps a seq high-water

- Status: Accepted
- Date: 2026-10-19
- Issue: #884
- Amends: [ADR-0068](0068-shared-per-session-seq-counter.md) (who owns the
  counter after a session's task exits, and which ids mint the seq-`0`
  sentinel), and the #884 known limit in the Consequences of
  [ADR-0237](0237-detached-flag-on-tool-request.md) and
  [ADR-0238](0238-compaction-moves-a-background-scripts-card.md)

## Context

ADR-0068 shares one seq counter per session between the session task and
the runtime. The session task registered it on start and removed it on exit
(`commands::retire`). After that, `Holly::emit_for_session` minted seq `0`.
ADR-0068 called this harmless: "there is no live content stream to collide
with".

Detached work broke that assumption. A `background: true` `rhai` script
([ADR-0185](0185-rhai-joins-background-and-poll.md)) outlives its turn, and
ADR-0232's `stop_attached` spares its cards when the session hibernates,
ends or closes. When the script parks a nested `Ask` after that, the card is
announced with seq `0`. A head that dedupes on `seq > last_seen` drops it.
The TUI's `fold_tool_request` is one of these heads. The card is never
shown, an attended park has no timeout, and the script waits forever. The
idle-TTL sweep ([ADR-0090](0090-idle-ttl-auto-hibernation.md)) makes this
easy to hit: a long background script parks after its idle session was
hibernated.

Seq `0` also breaks ADR-0068's invariant for every head, not only the TUI.
`(session, seq)` is no longer unique once two events share seq `0`, and seq
`0` is no longer greater than every seq already used.

## Decision

1. **Retiring a session keeps its counter's value.** `SeqRegistry`
   (`holly/seqs.rs`) holds two maps under one mutex: `live` (the running
   tasks' `Arc<AtomicU64>` counters) and `retired` (a high-water `u64` per
   id). `retire` moves a session from `live` to `retired`, keeping the
   counter's last value. This happens on hibernate, on end, and on close.
2. **A retired id keeps minting above its last seq.** `next` (used by
   `emit_for_session` and the supervisor's `Error` path) draws from the live
   counter if there is one. Otherwise it bumps the retired high-water and
   returns the new value. A card a detached script parks after its session
   went away gets a real seq, unique for the session and greater than every
   seq the session used before, so a `seq > last_seen` dedupe folds it.
3. **A resume continues above the high-water.** `register` (called by
   `start_session` before `SessionStarted`) removes the id's retired entry
   and calls `fetch_max` on the replay-seeded counter with it. So no seq is
   ever reused. This holds when the log has the seqs minted while the session
   was hibernated (the runtime's persistence tap does log them, since its
   root map outlives the session task). It also holds when the log is missing
   them: an embedder's own sink, a gap, or a resume that races the tap. A
   fresh spawn of a retired id (one that was never tombstoned) seeds above
   the high-water the same way.
4. **`is_live` reads only `live`.** A retired high-water is not liveness.
   `Holly::is_live`, and everything built on it (ADR-0238's
   park-forward walk stopping at a successor that is not live yet, the
   heads' lag recovery in `heads.rs`), keeps its meaning: started and not
   retired.
5. **An id that never started still mints `0`, and gets no entry.** Seq `0`
   stays the "no sequence" sentinel that ADR-0068 and
   [ADR-0072](0072-protocol-warts-settled-before-serve.md) define: no counter
   ever produces it (live seqs start at 1), so it can't collide with an
   authored seq, and heads render it through the seq-`0` bypass. Creating an
   entry for an unknown id would let any caller grow the map (a wire
   `Prompt` to a random id under `UnknownSession::Refuse`). It would also
   push real high-waters out of the bounded map.
6. **The retired map is bounded.** It holds at most `RETIRED_CAP` = 4096
   ids. Past that, the entry touched longest ago is evicted. A retire and
   every emit both count as a touch, so a script that is still emitting
   keeps its session's entry. An evicted id falls back to rule 5. A resume
   of an evicted id still seeds from its log, which holds every seq the tap
   persisted. An entry costs one id plus two `u64`s. The O(n) eviction scan
   runs only when the map is full.
7. **Retire and register are keyed by the counter, not only the id.** The
   supervisor sends `Hibernate` without waiting for the task to exit
   (`hibernate_subtree`). So a resume, a lazy spawn or an
   `UnknownSession::Resume` can start task B for the same id while the old
   task A is still tearing down, and B can register first. A task therefore
   retires its own `Arc` (`retire(session, &s.seq)`): the live entry is
   removed only when it is that same `Arc` (`Arc::ptr_eq`). Otherwise B
   stays live and only takes A's value with `fetch_max`. `register`, when it
   replaces a live entry, raises the new counter to at least the old one's
   value. Without this, A's late retire removed B's counter: `is_live` read
   false for a running session, and runtime emits minted from a stale
   high-water while B kept minting from its own `Arc`, which duplicated seqs.

## Consequences

- The seq a supervisor `Error` carries changes. One addressed to a retired
  id (a `Prompt` that raced behind its close, or an `UnknownSession::Refuse`
  of a hibernated id) now carries a real seq from the high-water instead of
  `0`. Heads already accept both: a real seq passes `seq > last`. Only an id
  that never started, or one evicted past the cap, still gets the seq-`0`
  error.
- The ADR-0237/0238 limit (#884) is fixed at the source: a detached card
  parked after a hibernate, an end or a close gets a real seq. The TUI needs
  no change. A retired session's view stays in `SessionRegistry` and keeps
  folding events, so the card is shown and answerable.
- Memory is bounded, not exact. Core cannot know when the runtime's last
  detached emit for an id has happened, so a retired entry is not freed the
  moment nothing will emit for it any more. It stays until the cap evicts it
  or a resume moves the id back to `live`. Exact reclamation would need the
  runtime to hold a lease (a handle that pins the high-water while detached
  work runs). That changes the `emit_for_session` API, and the cap makes it
  unnecessary for this fix. It is tracked as #891.
- A session that retired more than 4096 retirements ago, and whose detached
  script parks only now, gets seq `0` again. The cap is set so that this
  needs both a very long-lived engine and a script idle across thousands of
  other sessions' teardowns (#891). Eviction has a second effect: a later
  resume of that id seeds only from its log, so if the embedder's sink missed
  the events minted while it was retired, the resumed session can reuse
  their seqs.
- Rule 7 covers the registry's own entries only. A still holds its own
  `Arc` after B registered, so anything A mints from it before it exits
  (its cancelled round's tail) can still collide with B's sequence. Making
  the teardown and the restart of one id strictly ordered is #892.
- Two emitters for one session (the session task and a runtime service) can
  still broadcast their events out of seq order, because the seq is minted
  before the send and the two sends are not ordered. This is older than this
  ADR, which neither causes nor fixes it, and is tracked as #893.

## Alternatives considered

- **Keep the `Arc<AtomicU64>` alive forever.** This is the same as the
  high-water, but unbounded: every session ever started stays in memory.
- **Re-create the counter from a persisted high-water mark.** Core has no
  store ([ADR-0047](0047-local-trust-boundary.md)'s no-DB-in-core boundary),
  and asking the embedder's loader on every late emit would make
  `emit_for_session` async and fallible.
- **A runtime-local high-water, or a TUI bypass for seq-`0` cards.**
  ADR-0068 already rejected a runtime-local mark (it collides on resume). A
  head-side bypass would fix only the TUI, and every other head would still
  see a colliding, non-monotonic seq.
- **A lease held by the runtime's detached work.** This gives exact
  reclamation, but every detached emitter would have to carry a handle, and
  the high-water would still have to outlive the lease so a resume never
  reuses a seq. Deferred to #891, in case the cap ever proves too small.
