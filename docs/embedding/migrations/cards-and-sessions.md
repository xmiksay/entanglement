# Migration notes — cards, lineages & sessions (ADR-0233–0248)

> Part of the [migration notes](../migrations.md) of the [embedding guide](../../embedding.md).

## Live-only nested-call events (ADR-0233, #821)

- **Wire:** two new `OutEvent` variants, `NestedCall` (one binding call of a
  running `rhai` script: `parent_request_id`, 1-based `index`, `tool`, a
  bounded credential-free `args` summary, `duration_ms`, `is_error`) and
  `NestedCallSummary` (`suppressed`/`errors`: the calls past the 50-per-run
  cap, sent once at the script's end). Both carry `session` but no `seq`.
- **Live only:** `OutEvent::is_live_only()` is true for exactly these two.
  The persistence tap never hands them to a `RecordSink`, and replay ignores
  them, so a sink of your own needs no change. If you persist events outside
  the tap, skip `is_live_only()` events too.
- **Exhaustive matches** over `OutEvent` need the two new arms. Pair a report
  with its script by `parent_request_id`, never `request_id`.
- **Added:** `Holly::emit_live(ev)` broadcasts a live-only event and refuses
  (logs and drops) any other variant.

## Detached cards (ADR-0237, #872)

- **Wire:** `OutEvent::ToolRequest` and `PendingApproval` gain `detached:
  bool`, `true` on a background `rhai` script's nested card. It is
  serde-defaulted and omitted when `false`, so ordinary cards and old logs
  are unchanged. A head that drops its cards on a terminal `Status` should
  keep the detached ones: the script still waits on them, with no timeout
  in an attended mode.
  `Stop` unwinds them too.
- **API:** `PendingDecisions::register_approval(approval, detail)` takes the
  detached flag from `approval.detached`.
- **Constructing `OutEvent::ToolRequest` or `PendingApproval`** needs
  `detached: false`.

## Cards moved after a compaction (ADR-0238, #871)

- **Wire:** after a compaction, a background script's card is announced a
  second time: a fresh `ToolRequest` on the successor, with the same
  `request_id`, content and `detached: true`. `ListPending` then reports it
  under the successor, so a `root`-filtered listing of the successor finds
  it. A head that keeps cards per session should drop the predecessor's
  copy when the successor announces the same `request_id`
  (`SessionStarted.predecessor` names the pair).
- **Answering:** `Approve`/`Reject` reaches the moved card under the
  predecessor's id or the successor's; a cached predecessor id is never a
  dead card, and it never reaches a card the successor parked itself.
  `PendingDecisions::forget(session, id)` now removes only that exact key,
  and only once its waiter is gone.
  `Stop` on the successor unwinds the moved card. A session never shows
  two live cards with one id, so a `ToolRequest` whose id a head already
  queues for that session is the same card: replace it in place.
- **Duplicate parks:** `PendingDecisions::register`/`register_approval`
  let only a turn card supersede a live turn card under the same key (the
  earlier waiter resolves to `Decision::Stop`). Any other park sharing a
  live card's key, or its id on its session, gets an immediate
  `Decision::Reject` with a reason and emits no card.

## A card's end on the wire (ADR-0240, #883)

- **Wire:** a new `OutEvent` variant, `ApprovalResolved { session, seq,
  request_id, outcome }`, with `outcome` one of `approved` / `rejected` /
  `expired` / `stopped` (`ApprovalOutcome`). The runtime emits it whenever a
  `ToolRequest` card leaves `PendingDecisions` — a decision from any head, a
  `question_timeout` expiry, a `Stop`, a session end/hibernate (its turn's
  cards), a superseding re-offer — on the card's home session, always after
  the card. Drop the card by `request_id`, only past your seq watermark for
  that session, and along the session's compaction predecessors if you keep
  per-session copies. An end at seq 0 (its session has no sequence) is folded by id without moving the watermark. `ask_user`
  questions never get one.
- **Persisted:** it is a content event with a seq, so the persistence tap
  logs it and `ReplayFrom` returns it (a seq-0 end excepted: `History` keeps
  only records past `after_seq`); core replay ignores it. A sink of your
  own needs no change.
- **Exhaustive matches** over `OutEvent` need the new arm. A wire consumer
  built against an older `OutEvent` fails to parse the new `kind`: decode the
  `kind` tag first and skip kinds you do not know.
- **API:** `PendingDecisions::announcing(holly)` builds a registry that emits
  it (the executor's own); `PendingDecisions::default()` stays silent. If you
  park through `register_approval` and emit the `ToolRequest` yourself, emit
  it straight after registering, or a `Stop`/timeout in between reports the
  end before the card.

## Heals walk the whole compaction chain (ADR-0242, #888/#887)

- **API:** new `Holly::predecessors_of(id)` returns every session `id`
  succeeded, nearest first, read from the supervisor's directory (so it
  survives lost lifecycle events). It is the backwards counterpart of
  `successor_of`; `lineage_head` is its last element.
- **Answering:** `Approve`/`Reject` now reach a moved card under any session
  it was shown on: the key it was parked under, its current home, or any
  home a move took it off. A head holding a stale copy on a middle link of a
  chain of compactions is never left with a dead card.
- **Registry:** `PendingDecisions` keeps no lineage of its own any more. A
  detached card registered through `register_approval` is homed on its own
  key; only `park_approval_as` homes a detached card on the live end of
  core's chain.

## Linear lineages and live script authority (ADR-0243, #898/#899/#900)

- **Spawn:** `Spawn { predecessor: Some(p) }` is refused with a supervisor
  `Error` (on the new id and on `p`) when `p` already has a compaction
  successor. A lineage is linear; `ForkSession` is the way to branch.
- **Resume:** re-learning a log's `SessionStarted { predecessor }` edges no
  longer overwrites a recorded successor, nor prepends to a chain or a live
  session this process already knows. `successor_of` keeps the first link,
  and no known session's `lineage_head` moves.
- **Compacted-away sessions:** `Holly::resume` of a session this process
  knows was compacted away is refused with an `Error` naming the end of its
  chain ("session `p` compacted into `s`; resume `s`"). The runtime's
  `SessionStore::load_for_resume` refuses the same from the logs
  (`SessionStore::compacted_into`), so a fresh process refuses it too.
- **Your own store:** in a fresh process core refuses only edges it has
  seen. An embedder that resumes from its own store, or answers
  `UnknownSession::Resume` through a `SessionLoader`, must refuse an id that
  a later root's `SessionStarted.predecessor` names (return an error from
  `load`, or don't call `Holly::resume`), and point at the successor.
- **Runtime:** a running `rhai` script re-reads its session's mode before
  every nested call (blocking runs too) and drops a snapshot overlay entry
  once its compaction lineage's epoch moved (`ModeEpochs`). Its per-run
  approvals lapse on a mode change. A file backup a switched background
  script records is marked `adopted` on the manifest `Change`
  (serde-defaulted; older manifests read unchanged).

## Seq order, leases and one task per id (ADR-0244, #891/#892/#893)

- **`Session.seq`** is now `Arc<SeqCell>` (was `Arc<AtomicU64>`). Read it with
  `session.seq.current()` instead of `.load(Ordering::..)`. `SeqCell` is
  re-exported at the crate root.
- **New API:** `Holly::seq_lease(&id) -> SeqLease` pins a session's seq
  counter while it is held (drop it to release). Take one when detached work
  that may emit for a session after it retires starts, and drop it when the
  work ends; `skutter`'s background `rhai` scripts do. Without it, a retired
  session's counter is evicted once more than 4096 other sessions retire, and
  `emit_for_session` falls back to seq `0` (ADR-0239).
- **`emit_for_session`'s `make` closure runs under the session's emit lock**
  (so delivery order is seq order). Build the event in it; don't block, and
  don't call back into `Holly` from it (no `emit_for_session` for any
  session, no `seq_lease`, no `is_live`): those take the seq registry's lock,
  which a concurrent session start holds while it locks a cell, so the call
  can deadlock.
- **Behaviour:** a task started for a just-hibernated id (a `Resume`, a
  lazy prompt, a load, a `Spawn`) is live to the supervisor at once
  (routing, `ListSessions`, `session_info`) but waits for the old task's
  exit before it registers or emits, so `SessionHibernated` always precedes
  the resumed `SessionStarted`, and `Holly::is_live` reads `false` until
  then; what is sent to it meanwhile is handled in order once it starts. A
  close of a waiting task is a close of a hibernated id: no `SessionEnded`,
  and its buffered frames get no reply (a hibernate likewise ends it
  silently). A head needs no change.

## A decision only answers its own kind of card (ADR-0245, #895/#894)

- **Wire:** an `AnswerQuestion`/`RetractQuestion`/`ReplaceQuestion` aimed
  at an approval card's `request_id`, or an `Approve`/`Reject` aimed at an
  `ask_user` question's, is refused with an `OutEvent::Error` on the session
  it named. The card stays parked and listed, and no `ApprovalResolved`
  follows; before, the park unwound silently and the turn hung.
- **Questions:** drop an `ask_user` question when the `ToolOutput` with its
  `request_id` arrives (an answer from any head, a retract, a
  `question_timeout`). A `UserQuestion` for an id you already queue is a
  `ReplaceQuestion`: replace the entry, do not queue a second one.
- **API:** `PendingDecisions::resolve` returns `Resolution`
  (`Delivered`/`Absent`/`Misaimed { kind }`); a router of your own should
  report `Misaimed`. Parks receive `seam::ApprovalDecision` or
  `seam::QuestionDecision` via `Decision::into_approval`/`into_question`;
  `QuestionDecision::Refused { reason }` is a refused question park, which
  must still reply. An `Error` watcher of your own sees the
  refusal like any session error.

## Resumed sub-agents and the teardown tail (ADR-0246, #905/#906)

No API change; three behaviours an embedder can see.

- **Mode:** a resumed sub-agent starts in its nearest ancestor's current
  mode when that is stricter than the mode its log replays, on
  `EngineConfig::mode_ladder`. Without a ladder nothing is provably
  stricter and resumes behave as before; pass your ordering in
  `mode_ladder` to get the clamp. The switch arrives as the start-up
  `ModeChanged`, and a mode-scoped overlay is dropped with it.
- **Loads:** under `UnknownSession::Resume`, a load that finishes with an
  ancestor closed, or with the id itself or an ancestor hibernated while it
  ran (and still down), refuses each queued message with one `Error` (seq
  `0` unless the id has a retired counter) and starts nothing.
- **Teardown tail:** a resume behind a slow teardown now folds what the old
  task emitted from the moment the supervisor handed it its `Hibernate` —
  when the supervisor handles your `HibernateSession`, which is later than
  `holly.hibernate()` returning (that only queues the frame). Events the
  session emitted after you read your records but before that hand-over
  are not in the tail; read your records after the hibernate if you need
  them.

## Attended sessions wait for their approver (ADR-0247, #828)

- **API:** `ToolExecutorBuilder::attendance(Attendance)` takes the registry
  your heads register a `Watcher` in; without it no session is watched. A
  watched session (or a child of one) never collapses an `Ask`, its cards
  and questions have no deadline, and a watcher arriving cancels one.
- **API:** `PendingDecisions::await_timed`/`ask_user::run_ask_user` take an
  `attendance::Expiry` (`Expiry::after(d)` / `Expiry::never()`) instead of
  `Option<Duration>`; `serve::router`/`serve::serve` take the `Attendance`
  and an `Option<serve::Rejoin>` (a listing wakes a hibernated session).
- **Wire:** an `Approve`/`Reject` or question frame whose id no park holds
  gets an `OutEvent::Error` (it was dropped); report `Resolution::Absent`.
- **Grading:** unattended, a retry graded by the same scoped rule naming
  the tool (`bash(rm -r*)`) is a repeat, not only an identical argument.

## Importable rhai modules (ADR-0248, #835)

- **Added:** `skills::{module_dirs, RhaiModules, rhai_modules}`,
  `SkillRegistry::modules` (filled by `load_registry_in`: built-ins, plus
  the user dir under `UserLayer::FromEnv` only, plus the project dir), and
  `ToolExecutorBuilder::module_dirs(dirs)`, the module write floor (feed it
  `skills::module_dirs(root, &UserLayer::FromEnv)`; with nothing wired there
  is no floor), and `LiveDefinitions::with_sessions(store)`, the store whose
  scratch dir a reload's prompt context names (default: the project's).
- **Behaviour:** `import "<name>"` now resolves in every run, and the
  `rhai` guide's `load_skill` result ends with the module listing.

---

Previous: [Migration notes — one surface, binding path & skills (ADR-0226–0230)](one-surface-and-rhai.md) · Next: [TLS & dependency pinning](../tls-and-pinning.md)
