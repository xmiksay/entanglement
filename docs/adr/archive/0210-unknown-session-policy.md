# 0210. Unknown-session policy: `SpawnBlank` | `Refuse` | `Resume(loader)`

- Status: Accepted
- Date: 2026-10-10
- Amends: [ADR-0077](0077-session-hibernation-evictable-resumable.md) (a
  hibernated id can now be rebuilt on demand, not only by an explicit
  `Resume`), [ADR-0090](0090-idle-ttl-auto-hibernation.md) (idle eviction is
  safe for a multi-user embedder without a liveness mirror)

## Context

Any routable `InMsg` addressed to a session id with no live task lazily spawned
a **blank** root under the default agent (issue #721). For the single-user
in-tree heads that is the convenience that lets a `Prompt` create its own
session. For a multi-user embedder it is a correctness bug: after an idle-TTL
hibernation, chess-base's next `Prompt` ran on an empty context with
`user = None` — silently falling back to the house provider key — and the
blank session's records were appended to the old session's log, corrupting the
next real resume. Embedders worked around it by mirroring liveness off the
broadcast (`LiveSessions`) and calling `Holly::resume` before every send
(`ensure_target_live`) — racy (the idle sweep can evict between the check and
the send) and duplicated in every embedder.

## Decision

`EngineConfig::unknown_session: UnknownSession` decides what such a message
does:

- **`SpawnBlank`** — today's behavior and the **default**, so every in-tree
  head and existing embedder is unchanged. The #639 refusal of a known
  hibernated sub-agent child still applies.
- **`Refuse`** — spawn nothing; emit an `OutEvent::Error` naming the session on
  the seq-`0` lifecycle bypass ([ADR-0072](0072-protocol-warts-settled-before-serve.md))
  and drop the message.
- **`Resume(Arc<dyn SessionLoader>)`** — the embedder implements
  `async fn load(&self, &SessionId) -> anyhow::Result<Option<Vec<(Option<InMsg>, OutEvent)>>>`,
  returning exactly the records `Holly::resume` already consumes (no parallel
  type). `Some` → the supervisor's ordinary resume path runs (cascading over
  the still-live spawn sub-tree, [ADR-0112](0112-resume-cascades-over-the-spawn-subtree.md)),
  then the message is delivered; `None` → behaves as `Refuse`; `Err` → an
  `Error` event, nothing spawned. A known hibernated child is *not* refused
  here: resuming it is precisely what #639's refusal asked the caller to do.

**Coverage.** The policy applies to every message the supervisor routes to a
session task: `Prompt`, `ToolResult` (after the compaction-successor redirect,
[ADR-0205](0205-every-compaction-forks-a-successor-session.md)), `Stop`,
`PauseSession`/`ResumeSession`, `SetMode`, `SetModel`, `SetGeneration`,
`SetSessionMeta`, `SetToolOverlay`, `Oneshot`. Uniformly — a `Stop` to a
hibernated id under `Resume` loads it and stops an idle session, a harmless
no-op that keeps the rule one sentence long. It does **not** apply to:
`Spawn` and `Resume` (they create explicitly), `CloseSession` (tombstones
regardless), `HibernateSession` (no-op on a non-live id), and the
runtime-consumed or engine-global frames (`Approve`/`Reject`/`AnswerQuestion`/
`RetractQuestion`/`ReplaceQuestion`/`ReplayFrom`, `List*`, `Mcp*`), which never
reach session routing. A closed id is refused under every policy (ADR-0028).

**Concurrency.** The supervisor is one loop and must not await embedder I/O,
so a load runs on its own task and reports back over a supervisor-owned
channel (not the inbox, for the same keep-the-actor-alive reason as the
compaction fork channel). While it is in flight, `pending_loads[id]` holds a
queue: every further routable message for that id is appended rather than
starting a second load — exactly one load per id. On completion the queue is
replayed through the normal handler in arrival order. A message for an id
whose queue still exists is queued even if the id meanwhile went live (an
explicit `Resume`/`Spawn` racing the load), so it can't overtake earlier
messages; the loaded records are then discarded. `CloseSession` abandons the
load: queued messages are refused as closed and the completion applies
nothing. A panicking loader surfaces as `Err`; a loader that never returns
strands its id's queue, so the trait docs tell embedders to bound their I/O.
The per-id queue is **unbounded**: a slow loader plus a burst of frames for
that id grows memory until the load completes — another reason to bound the
loader. Load completions are polled ahead of the inbox in the supervisor's
`biased` `select!`, so a busy inbox can't starve them (ordering stays safe:
later frames for a loading id queue behind it).
A refusal emits one `Error` per dropped message so a head can account for
each send.

**`Holly::is_live(&SessionId) -> bool`** — a synchronous read of the
per-session seq registry (registration moved ahead of `SessionStarted`, so it
reads `true` from `SessionStarted` until `SessionEnded`/`SessionHibernated`).
It is advisory: the policy, not check-then-send, is the race-free way to
address a possibly-evicted session.

## Consequences

- chess-base/site drop `LiveSessions` + `ensure_target_live`: install
  `Resume(loader)` over their store and keep sending plain `Prompt`s; an
  evicted session comes back with its history and `user` intact.
- Inbound fan-out still happens on arrival, so a persistence tap records a
  queued `Prompt` before the resumed session re-announces `SessionStarted`.
  Replay scopes a prompt by its own session, not its paired event, so the log
  still folds correctly.
- **Security: the loader is reachable from the wire.** Every covered frame is
  wire-allowed (`Prompt`, `SetModel`, …), so under `Resume` a client relayed
  through `send_from_wire` triggers embedder loader I/O for **any** id it
  names: (a) each distinct id spawns its own loader task, with no concurrency
  cap in core; (b) cross-tenant — a resumed session restores its `user` from
  its own log, so a frame naming another tenant's **evicted** session revives
  it with *that* tenant's history and provider key (under `SpawnBlank` the
  same frame got a blank session with `user = None`). This widens
  [embedding.md](../embedding.md) §2's tenant-prefix caveat from live to
  evicted sessions. Rule: the head **must** validate tenant ownership of the
  id before `send_from_wire`. The loader receives no provenance and needs
  none when the head does; an embedder that wants a backstop can also rate-
  or tenant-check inside `load`.
- `entanglement-core` now depends on `async-trait` (previously dev-only) — the
  same shape as `Llm`.

## Alternatives rejected

- **Flip the default to `Refuse`/`Resume`.** Breaks every in-tree head's
  prompt-creates-session convenience for no single-user benefit.
- **A sync loader / snapshot cache** (the resolver seams' pattern). Loading is
  inherently I/O and happens once per eviction, not per turn; forcing a cache
  just moves the same async fetch into the embedder.
- **Awaiting the loader inline in the supervisor.** One slow store read would
  stall routing for every other session.
- **Only `Prompt` triggers the policy.** A `SetModel` or `ToolResult` to an
  evicted id would then still blank-spawn — the exact bug, by a side door.
