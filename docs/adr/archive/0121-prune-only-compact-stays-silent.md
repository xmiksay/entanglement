# 0121. Prune-only `Context::compact` stays silent — accepted live/replay divergence

- Status: Accepted
- Date: 2026-07-20
- Relates to: [ADR-0103](0103-auto-summarize-on-context-overflow.md)

## Context

[ADR-0103](0103-auto-summarize-on-context-overflow.md) gave the turn loop's
context-overflow guard (`session/turn.rs::enforce_context_window`, #178) three
recovery steps: auto-summarize in place (emits `OutEvent::Compacted { auto:
true, .. }`, replayed via `Context::apply_compaction`), fall back to
placeholder-pruning the oldest tool outputs (`Context::compact`), then refuse
the turn. The prune fallback fires whenever auto-summarize is disabled
(`EngineConfig::auto_compact = false`), its own guard trips (an oversized
transcript/tail, an LLM error, a truncated summary), or the summarized result
still doesn't fit.

`Context::compact` mutates `Session.ctx` in place — rewriting the oldest
`Tool`-role messages to a short placeholder — but emits no `OutEvent`. Nothing
in the persisted log records that the prune happened. `Session::replay` (which
folds the log back into a fresh `Context` on resume) therefore never replays
it: a resumed session reconstructs the *full*, unpruned tool outputs the live
session had already discarded. The resumed session's history is bigger than
what the live session actually sent to the model for the rest of that turn —
an undocumented divergence in the exact request shape between a live run and
its replay.

In practice this is self-correcting: `enforce_context_window` runs before
*every* round, so a resumed session that is still over budget just re-prunes
(or re-summarizes) on its very next turn, converging to the same place the
live session was. This is the same reasoning `Session::replay`'s
`OutEvent::Compacted { auto: false, .. }` arm already relies on for the manual
`/compact` op's pre-ADR-0101 legacy records (see the comment there): a
recorded-but-unmutated compaction is deliberately left for the guard to
re-derive, not replayed literally.

## Decision

**Leave `Context::compact` as a silent, no-event mutation. Do not add a wire
event (or extend `Compacted`) for the prune-only fallback.**

The three recovery steps stay asymmetric on purpose:

- **Auto-summarize** (`apply_compaction`) replaces the *entire* history with
  one summary message plus a verbatim kept-tail — a destructive rewrite an
  LLM produced that cannot be recomputed from the raw log alone, so it must be
  recorded (`Compacted { auto: true, .. }`) for replay to reconstruct the same
  `Context` shape.
- **Prune** (`compact`) is a deterministic, idempotent function of the
  existing history and the model's token budget alone — no LLM call, no
  information destroyed that isn't already implied by "this transcript is
  over budget for this model". `enforce_context_window` re-derives the
  identical result from the full log on every subsequent round (replay
  included), so recording it buys replay-fidelity in the exact request bytes
  at the cost of a new wire concept, for a state that self-heals within one
  round-trip.

This is documented, not silently accepted: `Context::compact`'s doc comment,
`session/turn.rs::enforce_context_window`'s doc comment, and
`docs/architecture/engine.md`'s auto-summarize section now say explicitly that
the prune step is unrecorded and why.

## Consequences

- **Positive.** No new wire variant / field for a divergence that is benign
  and self-correcting — keeps `OutEvent::Compacted` meaning one thing
  (a *reconstructable-only-from-the-event* history rewrite), not two
  different mutation shapes told apart by an `auto` flag that would then also
  have to distinguish "replay via `apply_compaction`" from "replay via
  `compact`".
- **Positive.** Matches the precedent already set for `auto: false` legacy
  records in `Session::replay` — "recorded state that a subsequent guard
  re-derives is left for the guard to re-derive" is now a consistent rule
  across both compaction paths, not an inconsistency between them.
- **Negative / accepted cost.** A resumed session's first turn or two may
  re-run pruning (or auto-summarize) that the live session already paid for,
  sending a larger request than the live session's steady state and then
  reconverging. Bounded: it is the same recovery path either way, resolves
  within the guard's own next invocation, and never sends an over-window
  request (the guard runs before every round). No user-visible effect beyond
  a possibly-recomputed placeholder or an extra summarization round-trip.
- **Neutral.** If a future need arises for byte-exact live/replay parity on
  every mutation (e.g. an embedder auditing exact token spend per historical
  turn), this decision is the place to revisit — the fix direction survives
  in the closed issue (#450) as "emit a `Compacted`-family event for the
  prune, with `replay` calling `compact()` instead of `apply_compaction`".

## Alternatives considered

- **Emit `Compacted { auto: true, summary: "", kept: .. }` for the prune,
  reusing the existing event.** Rejected: `Session::replay`'s `auto: true` arm
  unconditionally calls `apply_compaction`, which *replaces the whole history
  with a single summary message* — exactly the destructive rewrite the prune
  path does not do. Reusing the event without a way to tell the two mutation
  shapes apart would make replay wrong in a new way (collapsing history that
  was only placeholder-pruned, not summarized), trading a benign divergence
  for an actual data-loss bug.
- **Add a new `OutEvent` variant (or a `kind` field on `Compacted`) for the
  prune, and teach `replay` to call `Context::compact()` in that arm.**
  Viable, and the honest full fix — but it adds a permanent wire/protocol
  surface (a new event variant every head must at least pattern-match through,
  [ADR-0072](0072-protocol-warts-settled-before-serve.md)'s "wire settled
  before `serve` freezes it" now behind us) to close a gap that already
  self-heals in one round-trip and costs nothing but a possibly-redundant
  prune. Deferred rather than rejected outright: worth doing if the ledger
  (`docs/deferred-work-ledger.md`) later shows an embedder actually needs
  exact live/replay parity here.
