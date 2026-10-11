# 0223. Non-compacting session fork (`/fork`)

- Status: Accepted
- Date: 2026-10-11
- Relates to: [ADR-0205](0205-every-compaction-forks-a-successor-session.md)
  (the compaction fork this reuses the channel of),
  [ADR-0101](0101-compaction-forks-into-a-new-session-copy-on-write.md)/[ADR-0110](0110-compaction-successor-closes-predecessor.md)
  (copy-on-write successors), [ADR-0211](0211-idgen-context-and-tenant-prefix-inheritance.md)
  (tenant prefix), [ADR-0212](0212-undo-rewind-via-per-write-file-backups.md)
  (turn numbering, per-root backups), [ADR-0069](0069-trusted-untrusted-wire-frame-split.md)
  (wire allowlist)

## Context

#731 asked for a TUI `/fork`: branch a conversation to try something else
without losing the original. Every existing fork is a compaction (ADR-0205):
it seeds a successor from a *text* prompt (a summary or a pruned transcript)
via `Spawn { predecessor }` and retires the source. Nothing could start a
session from a verbatim copy of another session's `Context` — messages, tool
round-trips, reasoning blocks — and keep the source running (#786).

## Decision

1. **Protocol.** A trusted-only `InMsg::ForkSession { session, at_turn? }`.
   It is **wire-refused**, like `Spawn`: it mints a session. The fork announces
   itself with its ordinary `SessionStarted` (a root: `parent` and
   `predecessor` both `None`), then `OutEvent::SessionForked { session, seq,
   forked_from, at_turn?, context }`, emitted by the fork before anything else
   it logs. `predecessor` is not reused: ADR-0205 gives it "the source is
   retired" semantics (late tool results redirect to the successor), which a
   fork must not inherit. The lineage rides on the new event rather than a new
   `SessionStarted` field, so the event every resume re-emits stays small.
2. **Seed.** `context` is the verbatim message list the fork starts from, and
   `Session::replay` folds it by putting it ahead of the fork's own history.
   The fork's log is therefore self-contained and replays exactly. It works
   for every `RecordSink` with no runtime log copying, and it cannot race a
   persistence tap that has not yet flushed the source's tail. The history is
   taken from the source's live `Context` in process. It rides the
   session→supervisor fork channel (ADR-0205) on `Inherited::seed`, beside a
   `Spawn` with an empty prompt. The persistence tap now logs no prompt for an
   empty `Spawn` prompt (no turn is queued, so none would replay). That also
   fixes ADR-0174's identity-binding spawn, which used to replay a blank user
   message.
3. **`at_turn`.** `None` copies everything; `Some(n)` keeps turns `1..=n`. A
   turn is a user prompt applied to the context, counted the way ADR-0212's
   `/rewind` counts them: `Context` records each prompt's index as it is
   pushed, live and on replay. The cut falls just before turn `n + 1`'s
   prompt, a turn boundary, so no tool round-trip is split. A number past the
   last turn copies everything. A fork does not count its seed as turns: its
   own prompts start at 1, matching its own log, which is what `/rewind`
   reads. So `Some(0)` on a fork keeps exactly its seed.
4. **Inherited state.** The fork gets a fresh id minted through
   `IdContext::successor_of(source)`, so the default generator keeps the
   `{tenant}:` prefix. It keeps the source's agent and owning user, its live
   bound backend and generation (`Inherited`, #768, carried and not
   re-resolved), its mode (the `Spawn`'s explicit mode), and its tool overlay
   (#776).
5. **What is not carried.** `ForkSession` is deferred while a turn is live or
   the session is paused, like `RevertFiles`, so a fork is always cut at rest.
   It never carries a parked tool batch or a half-streamed round, and it
   starts idle. Spawned children are not cloned: the fork is a root with none.
   File backups are per root (ADR-0212), so the fork's `/undo` history starts
   empty, and an edit made before the fork can only be reverted from the
   source.
6. **Source untouched.** Nothing is emitted on the source, and its `Context`
   is only read. The `ForkSession` frame itself lands in the source's log as
   an inbound record that replay ignores.
7. **TUI.** `/fork [N]` sends `ForkSession` for the active session. A
   `SessionForked` whose source is the active session moves the user into the
   fork. The fork's view opens on a lineage notice and the inherited
   transcript rebuilt from `context`, live or restored from its log, and the
   sessions list tags it "⑂ forked from …".

## Consequences

- A fork's log stores its seed history once. A long session forked many times
  stores that history once per fork. This is the price of logs that are
  self-contained, which compaction successors have too.
- `ListSessions`/`SessionInfo` carry no fork lineage. Only the `SessionForked`
  event and the persisted log do.
- `serve`/`pipe` cannot fork. An embedder forks over `Holly::send`.

## Rejected

- **Copying the source's log records into the fork's file.** It is
  runtime-only (an embedder's sink would get nothing), it needs every
  record's session id rewritten, and it races the persistence tap's lag on
  the source's tail.
- **Reusing `predecessor`.** It means "the source is retired" and drives
  tool-result redirection.
- **A `forked_from` field on `SessionStarted`.** It would touch every
  construction site of a hot event, and the seed needs its own event anyway.
- **Forking mid-turn at the last settled boundary.** That needs a separate
  in-flight-turn cut. Deferring gives the same "idle, whole turns" fork with
  no new state.
