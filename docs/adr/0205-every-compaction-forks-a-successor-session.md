# 0205. Every compaction forks a successor session; no log is ever rewritten in place

- Status: Accepted
- Date: 2026-09-16
- Issue: #560 (pre-release audit umbrella)
- Relates to: [ADR-0101](0101-compaction-forks-into-a-new-session-copy-on-write.md)
  (manual `/compact`'s copy-on-write fork, generalized here to every
  compaction), [ADR-0103](0103-auto-summarize-on-context-overflow.md)
  (**amended**: auto-summarize no longer mutates the live context),
  [ADR-0121](0121-prune-only-compact-stays-silent.md) (**superseded**: the
  prune-only fallback forks and announces itself like any other compaction),
  [ADR-0110](0110-compaction-successor-closes-predecessor.md) (the
  successor-closes-predecessor lifecycle, now the only compaction lifecycle),
  [ADR-0102](0102-compact-keep-tail-verbatim-in-the-fork-prompt.md) (the kept
  tail, unchanged), [ADR-0202](0202-prompt-cache-discipline-anchors-deferral-replay-compaction-date.md)
  (the compaction request shape and the replay-fidelity rule this completes).

## Context

Three compaction paths had three lifecycles. Manual `/compact` forked a
successor session copy-on-write and retired the source (ADR-0101). Automatic
summarization on overflow mutated the live `Context` in place (ADR-0103).
The prune-only fallback mutated in place *and* emitted nothing at all
(ADR-0121).

Both in-place paths rewrite a session's history behind its own log. The
persisted event stream records what the model was sent before the mutation,
never the mutation itself for the prune case, so a resumed session cannot
reconstruct what the live session actually held: ADR-0202's replay-fidelity
work made every other path byte-exact and left these two as the remaining
divergence. They also destroy the pre-compaction conversation: once the live
context is rewritten there is nothing to go back to, while a manual compact
leaves the source session intact and resumable.

## Decision

**Compaction never mutates a live session.** Every compaction — manual
`/compact`, automatic summarization on context overflow, and the prune-only
fallback — produces a **successor session** seeded from the summary (or the
pruned head) plus the verbatim kept tail (ADR-0102), and **retires the
source session unchanged** at the fork point (ADR-0110's lifecycle, now
universal).

- **The prune-only fallback announces itself.** It emits the same
  `Compacted` event the other paths do, because a head must follow the fork.
  ADR-0121's silence is retired: it existed to avoid a confusing notice for
  a no-summary path, and a session that silently forks is worse.
- **A mid-turn fork carries the turn.** Overflow can trigger between rounds
  of a turn whose tool batch is parked. The successor inherits the parked
  `TurnState`, the turn continues there, and results arriving for that batch
  resolve against the successor. The source session's log ends exactly at
  the fork.
- **Every session log is therefore append-only and exactly replayable.** No
  path rewrites history behind the log, so the ADR-0202 byte-equality
  guarantee holds for compacted sessions too.

## Consequences

- A user keeps the pre-compaction session: it is retired, not destroyed, and
  stays resumable and inspectable, which is what `/compact` users already
  had and overflow users did not.
- Heads must follow a session id change they did not ask for. The manual
  path already does this; the automatic paths now use the same events, so
  the handling is shared rather than duplicated.
- Session count grows: a long-running session that overflows repeatedly
  leaves a chain of retired predecessors. The startup retention prune
  (`ENTANGLEMENT_SESSION_RETENTION_DAYS`) already bounds that on disk.
- The prune-only path becomes visible in the TUI and in `sessions` listings;
  a head that ignored `Compacted { auto: true }` before now sees one where
  it previously saw nothing.
