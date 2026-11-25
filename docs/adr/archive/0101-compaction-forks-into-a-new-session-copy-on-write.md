# 0101. Compaction forks into a new session (copy-on-write)

- Status: Accepted; its "Keep-tail under copy-on-write" rejected alternative
  (below) is revisited by
  [0102](0102-compact-keep-tail-verbatim-in-the-fork-prompt.md) — that
  rejection was scoped to the *source* (which does keep everything, unchanged
  here), not the *fork*'s fidelity, which 0102 addresses without touching
  anything decided in this document; amended by
  [0110](0110-compaction-successor-closes-predecessor.md) — the fork now
  *retires* the source session (successor closes predecessor), so the
  source-kept-open implicit undo described here is no longer reachable
  interactively
- Supersedes [0082](0082-single-shot-session-ops-and-persisted-compaction.md)
  (the `InMsg::Oneshot` wire shape and `OutEvent::Compacted` variant stay; the
  in-place `apply_compaction` mutation is removed)
- Date: 2026-07-16
- Issue #324 (follow-up): the "model truncated" / "cannot continue" failure mode
  where `/compact` destroyed the live history.

## Context

ADR-0082 shipped `/compact` (`InMsg::Oneshot { op: "compact" }`) as an **in-place**
mutation: `compact_op` called `Context::apply_compaction(&summary, 0)`, which does
`self.clear()` on the only copy of the live history and replaces it with the
summary. Two gaps in that design combined into a data-loss bug:

1. **Truncated summaries were not refused.** `compact_op` did not check the
   summary's `StopReason` — unlike `turn.rs`, which surfaces a `MaxTokens`-cut-off
   reply as a recoverable warning. When the summary hit the output token limit,
   the entire conversation was replaced with a cut-off fragment and the session
   could not continue.
2. **The mutation is irreversible.** `apply_compaction` is a `clear()` — there is
   no undo. A botched summary (truncated or merely bad) permanently destroyed the
   original history. The `Compacted` event was persisted, so even resume could not
   recover it: `Session::replay`'s `Compacted` arm re-applied the same
   `apply_compaction`.

The user's instinct — "fork the summary into a new session" — is the right fix:
**copy-on-write** means a bad compact produces a throwaway new session, never a
destroyed original.

## Decision

`/compact` is now **copy-on-write**: the source session's `Context` is **never
mutated**. The summary rides only in the `OutEvent::Compacted` event, and the head
that issued the compaction forks it into a new session.

### Phase 1 — refuse bad summaries (necessary regardless of fork)

`session/ops.rs`, `compact_op`:

1. **Refuse a truncated summary.** After `oneshot_text` returns `Ok((summary,
   finish))`, if `finish`'s `StopReason` is `MaxTokens`, emit `Error` and return —
   no fork, no mutation. Mirrors `turn.rs`'s truncation surfacing.
2. **Guard oversized transcript input.** Before building the summarization
   request, estimate whether the rendered transcript fits `s.ctx.limit()`. If it
   overflows, emit `Error` instead of shipping a request the provider will 4xx.

### Phase 2 — fork semantics (the safety win)

3. **Stop mutating the source.** `compact_op` no longer calls
   `apply_compaction`. `OutEvent::Compacted { session, seq, summary, kept }` is
   now a *report* ("summary ready, source untouched"), not a confirmation of
   mutation. The source `Context` is left unchanged.

4. **The TUI head forks on `Compacted`.** On receiving `Compacted`, the TUI mints
   a fresh id, sends `InMsg::Spawn { session: new_id, parent: source_id, agent:
   source_profile, prompt: summary }` via `Holly::send`, and switches its active
   view to `new_id`. `Spawn` already inherits the profile/model pin, seeds the
   summary as the first message, and records lineage (the fork is a child of the
   source). The source stays in the session list, idle, independently resumable.

   This is a two-phase collaboration: a session task cannot create sessions
   (only the supervisor can, via `InMsg`), so `compact_op` reports and the head
   forks. The TUI records a pending `Spawn` on the synchronous `App`
   (`pending_compact_fork`); the async main loop (which holds `Holly`) drains it.

5. **`Session::replay`'s `Compacted` fold is a no-op.** The source is never
   mutated, so there is nothing to fold. A record written under the old in-place
   design is simply ignored — replaying it would clobber the full pre-compaction
   history the log still holds, which is exactly the history the source session
   should recover with.

6. **The stdio `run` head** (one-shot) prints the summary as before; since the
   source isn't mutated and the process is exiting, no fork is needed there.

### What is unchanged

- The **wire shape** is unchanged: `InMsg::Oneshot { op, args }` and
  `OutEvent::Compacted { session, seq, summary, kept }`. `kept` is now wire-legacy
  only (always `0`), retained for deserializing older records written under
  ADR-0082.
- `Context::apply_compaction` still exists (used by its own unit tests); it is no
  longer called by any live or replay path.
- The old prune-only `Context::compact` (#178) — the automatic pre-round fallback
  — is unchanged.

## Consequences

- **The original is always recoverable.** Even a *successful* summary never
  touches the original; you can always go back to the source session. A botched
  (truncated) summary is rejected outright, so it never forks either.
- **`/compact` now costs two sessions** (the preserved source + the fork). This
  is the explicit trade-off: safety over tidiness. The source can be closed
  manually (`CloseSession`) once the fork proves useful.
- **Resume of a compacted source** reconstructs the *full* pre-compaction history
  (the `Compacted` record is a no-op fold), so the source is always resumable to
  its complete state — the implicit undo.
- **Keep-tail (`kept > 0`)** remains pinned at `0`. Under copy-on-write there is
  no tail to keep (the source keeps everything), so the field is inert. It stays
  on the wire for backward compatibility only.

## Rejected alternatives

- **In-place mutation + refuse truncation only** (Phase 1, the minimal fix). It
  stops future botched compacts but the original's history is still replaced on
  every *successful* compact — the safety win is partial. The fork (Phase 2) is
  the real fix.
- **Fork from inside the session task.** Session tasks cannot create sessions —
  only the supervisor can, via `InMsg` (`Spawn`/`Resume`). The session task has
  no `Holly` handle and no way to mint a sibling. The two-phase report-then-fork
  is forced by the actor model, not a choice.
- **Mutate the source and emit a separate "forked" event.** Rejected: the whole
  point is that the source is *never* mutated, so a mutation event would be a
  lie. `Compacted` becomes a report; the fork is head policy.
- **Keep-tail under copy-on-write.** There is no tail to keep — the source
  retains the whole history. `kept` is inert and stays only for wire
  compatibility.
