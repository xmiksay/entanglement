# 0077. Session hibernation — evict in-memory state without tombstoning, resumable later

- Status: Accepted
- Date: 2026-07-15
- Adds a third session-lifecycle state between the `live` task+`Context` and the terminal tombstone of [`CloseSession`](0028 is pre-repo; see #105). Builds on the event-log persistence seam of [0061](0061-parked-turn-state-batch-tool-resolution.md) and its in-process re-offer sibling [0071](0071-parked-turn-reoffer-timer.md); trusted-only like the internal `Resume`, mirroring the frame split of [0069](0069-trusted-untrusted-wire-frame-split.md). Part of #307. Issue #318.

## Context

A long-lived embedder that holds one `Holly` for months — a server multiplexing
many users' sessions — accumulates every session ever touched: the supervisor
keeps one task + `Context` per `SessionId` for the life of the process. The only
lifecycle exit is `CloseSession`, which **tombstones** the id (session ids are
single-use, #105): a closed id can never be resumed until a full engine restart
clears the tombstone set. So the embedder is forced into the worst of both — keep
paying memory for a session it might resume, or close it and lose the ability to
resume it at all. Memory grows monotonically.

The embedder's persistence is already the source of truth: the event log in its
DB (or the CLI's `session_store` files) fully describes a session, and
`Holly::resume(root_id, records)` rebuilds the in-memory `Session` from it —
including a turn parked mid-approval, whose pending `ToolExec`s are re-offered
(ADR-0061/0071). The in-memory session is just a **cache** of that log. A cache
should be evictable.

## Decision

**Add a third lifecycle state — `hibernated` — reached by a new trusted-only
`InMsg::HibernateSession`. It tears the session down and releases its memory
*without* recording a tombstone, so the id stays resumable via the existing
`Holly::resume` path. Core snapshots nothing; rebuilding is the embedder's job
from its log.**

Three states, made explicit:

| State | In-memory | Id tombstoned | Resumable |
| --- | --- | --- | --- |
| **live** | task + `Context` | no | — (already live) |
| **hibernated** | none | **no** | **yes** (`resume`) |
| **closed** | none | yes | no (refused) |

### Protocol (`entanglement-core::protocol`)

- `InMsg::HibernateSession { session }` — embedder-initiated. It is **not**
  wire-allowed (`wire_allowed()` refuses it alongside `ToolResult`/`Spawn`/
  `Resume`): a wire head must not be able to evict another session's in-memory
  state. `Holly::hibernate(session)` is the privileged convenience wrapper,
  sibling to `Holly::resume`.
- `OutEvent::SessionHibernated { session, ts }` — a lifecycle event (no `seq`),
  distinct from `SessionEnded` so heads and persistence taps can tell eviction
  from termination. The session task emits it for itself, exactly as it emits
  `SessionEnded`.

### Supervisor (`holly.rs`)

On `HibernateSession`, cascade over the spawn sub-tree (the same
`collect_subtree` walk `CloseSession` uses — a leftover descendant would keep
burning tokens with no consumer), and for each victim:

- send `SessionCmd::Hibernate` to the task, then **drop** its command sender;
- remove it from the `sessions` map, `session_meta`, and `parent_links`
  (memory released, gone from `ListSessions`);
- **do not** insert it into `closed` — the only difference from the
  `CloseSession` handler, and the whole point.

Sending the command *then* dropping the sender is deliberate: a buffered command
is delivered before the channel-closed `None`, so the task tears down via
`SessionHibernated`; and the sender-drop is what unwinds a turn parked
**mid-stream** (see below).

### Session task (`session.rs`)

`SessionCmd::Hibernate` is handled in the main loop like the inbox-close `None`
arm — drop the shared seq counter, emit the lifecycle event, return (dropping
`Session`, i.e. the `Context`/history) — but emitting `SessionHibernated` instead
of `SessionEnded`. Reached in three situations, all correct:

- **Idle**: received directly, torn down immediately.
- **Parked on approval**: received via the parked `recv` (the ADR-0071 re-offer
  `timeout` path). Safe by construction — the pending `ToolExec`s are in the
  embedder's log, and resume re-offers them (same `request_id`, fresh `seq`).
- **Mid-stream**: the streaming round's `select!` receives `Hibernate` and
  stashes it (the existing non-`Stop` branch); the sender-drop then makes the
  next `rx.recv()` return `None`, which cancels the round (ADR-0017 cancel
  semantics — `Context` preserved, nothing committed); the idle loop then pops
  the stashed `Hibernate` and tears down. This is **stop-then-hibernate**.

### Runtime (`tool_runner.rs`)

The executor releases a hibernated session's bookkeeping (in-memory grants,
cancel handles, the re-offer dedupe set) on `SessionHibernated` exactly as it
does on `SessionEnded` — the session is gone from memory either way; a resume
rebuilds what it needs.

## Safety: hibernating an active turn

The ADR settles the two questions #318 raised:

- **Mid-streaming turn → stop-then-hibernate** (not refuse). The in-flight round
  is discarded, which is **lossless w.r.t. the embedder's log**: a mid-stream
  tail is text-only (no `ToolCall` is emitted until a round completes), and
  `Session::replay` already drops a text-only tail — the live engine never
  committed it either. So resume rebuilds the session to its last *committed*
  state, identical to a crash at the same instant. Refusing was the simpler
  alternative the issue floated, but it needs the supervisor to know a session's
  turn state (it doesn't) or a back-channel from the task; stop-then-hibernate
  composes cleanly with immediate map removal and loses nothing the log kept.
- **Parked-on-approval → safe** either way, thanks to re-offer (ADR-0061/0071).

**In-flight inbox ordering** falls out of the existing serial-per-session
routing: the supervisor processes one frame at a time, and the session task
drains its command channel in order, so any frame already routed ahead of the
`Hibernate` is handled before teardown; frames that arrive after the map entry is
gone hit no session (a `Prompt` for a hibernated-but-not-closed id lazily
*would* respawn a blank session — the embedder is expected to `resume` before
re-prompting, per the #315 embedder guide).

## Consequences

- A server embedding one `Holly` can cap memory across many users: turn reaches
  `Done` → after an idle window the embedder sends `HibernateSession` → on the
  next prompt/approve it reads the session's events from its DB, `pair_records` →
  `resume` → delivers the prompt. Session-list/history UIs render from the DB and
  never touch the engine.
- Hibernate → resume → continue is **context-identical** to a never-hibernated
  control (test: the provider sees the same `messages`), because resume replays
  the same log the live session produced.
- Closed ids stay terminal: `resume`/`Spawn`/lazy-`Prompt` still refuse a
  tombstoned id. Hibernation is orthogonal — it never touches `closed`.
- **Non-goal:** core does not snapshot or restore state itself. Hibernate is pure
  eviction; the rebuild is the embedder's log replay. This keeps the "no DB in
  core" boundary (ADR-0061) intact.
- `EngineConfig.idle_ttl` auto-hibernation (issue #318 item 4) is **not** taken
  here. It needs the supervisor to observe *outbound* activity (a long streaming
  turn has no inbound traffic) to avoid evicting a busy session, which the
  supervisor does not subscribe to. Left to the embedder, which has the policy
  context ("only after `Done`, never mid-approval") and already drives
  `HibernateSession`. Revisit if a common policy emerges.

## Alternatives rejected

- **Reuse `CloseSession` and clear the tombstone on resume.** Conflates two
  distinct intents — "this id is spent" vs "evict but keep it" — and would let a
  resume silently resurrect an id the embedder meant to retire (the #105 bug). A
  separate message keeps the tombstone contract sharp.
- **Refuse hibernate during an active stream.** Simpler-sounding, but needs turn
  state the supervisor lacks or a task→supervisor back-channel to coordinate map
  removal with the refusal; and it loses nothing over stop-then-hibernate, since
  the discarded round was never persisted anyway. The embedder can still race
  `Stop` first if it wants a clean park.
- **Core-side snapshot/restore.** Would duplicate the embedder's event log inside
  core and re-introduce persistence the three-layer split deliberately keeps out
  (ADR-0061). Eviction + replay reuses one seam instead of adding a second.
