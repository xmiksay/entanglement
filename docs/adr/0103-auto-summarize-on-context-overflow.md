# 0103. Auto-summarize on context overflow, in place — distinct from `/compact`'s copy-on-write

- Status: Accepted
- Date: 2026-07-17
- Issue #398 (part of #396); composes with #397/[ADR-0102](0102-compact-keep-tail-verbatim-in-the-fork-prompt.md)

## Context

`entanglement-core/src/session/turn.rs` already guards every round against the
model's context window (#178): over budget, it calls `Context::compact`
(placeholder-prune the oldest tool outputs) and, failing that, refuses the
turn. Pruning is free but lossy — the model loses the pruned content outright,
just a stable placeholder string.

Separately, `"compact"` (`InMsg::Oneshot`, ADR-0082) asks the model itself to
summarize the history — far more information-preserving — but is manual-only,
and (ADR-0101) **copy-on-write**: the source `Context` is never mutated, the
summary rides in a report event, and a head forks it into a new session. A
long unattended session that overflows its window between user turns
therefore still degrades straight to lossy placeholder pruning, unless a user
remembers to run `/compact`.

Copy-on-write doesn't fit here. A turn overflowing mid-flight has no head
available to fork into a new session and redirect the user to — the turn
itself must proceed, in the same session, on the same request. The only
sound move is to compact `Context` **in place**.

## Decision

1. **`Context::within_limit()` failing in `run_round` first tries an
   in-place LLM summary**, gated by a new `EngineConfig::auto_compact: bool`
   (default `true`). It reuses the same rendering/guard/summarize logic
   `compact_op` uses (extracted to `session/summarize.rs`, `pub(crate) async
   fn summarize(...)`, parameterized over `Context`/`Llm`/model/generation/
   `requested_kept`/instructions so both callers share one implementation),
   requesting a small fixed keep-tail (`AUTO_COMPACT_KEEP_TAIL` messages,
   clamped to a safe boundary by the existing `Context::safe_kept` exactly as
   #397/ADR-0102 does for the manual path) so the turn's own most recent
   exchange isn't paraphrased away.
2. **On success, `Context::apply_compaction` mutates the session's history in
   place** — the one production call site `apply_compaction` has had since
   ADR-0101 made the manual path copy-on-write and left it otherwise dead
   (kept only for its own unit tests). This is the fundamental split from
   `/compact`: auto-summarize is a *turn-loop recovery mechanism*, not a
   user-facing report-and-fork.
3. **`OutEvent::Compacted` gains `#[serde(default)] auto: bool`.** `false`
   (the default, matching every existing writer) is `/compact`'s copy-on-write
   report; `true` marks an in-place auto-compaction. Additive, no wire break.
4. **Replay honors the split.** `Session::replay`'s `Compacted` fold stays a
   no-op for `auto: false` (ADR-0101's reasoning — nothing to fold, the
   source was never mutated — still holds). For `auto: true` it flushes
   whatever pending assistant/tool state the fold has accumulated so far (same
   flush the `Done` arm already does) and then calls `Context::apply_compaction`
   with the record's `summary`/`kept` — reconstructing the exact in-place
   mutation the live engine performed, so a resumed session's history matches
   the live one instead of silently recovering the pre-compaction log tail.
5. **Heads must not fork on `auto: true`.** The TUI's `handle_compacted` only
   forks a new session for `auto: false`; on `auto: true` it renders an
   in-place notice on the *same* view (no new session, no `Spawn`) — the
   session already continued under the reduced context by the time the event
   reaches the head. The stdio `run` head's one-line summary render likewise
   branches on `auto` instead of always suggesting a fork.
6. **Fallback chain, not a hard switch.** If auto-summarize is disabled, its
   own guard trips (transcript still too big, an oversized kept tail, an LLM
   error, or a truncated summary), or the result still doesn't fit, `run_round`
   falls through to the existing `Context::compact` prune, and only refuses
   the turn if that *also* fails — byte-identical to the pre-#398 behavior
   when `auto_compact` is off or every guard trips. No recursive
   summarize-then-still-overflow loop: each round attempts the ladder
   (summarize → prune → refuse) exactly once: since `apply_compaction`
   collapses the head to one dense summary message, the result is
   overwhelmingly smaller than the budget, and `max_turns` (#177) remains the
   backstop for any turn that still can't converge.

## Consequences

- **Long unattended sessions degrade gracefully by default** — a dense LLM
  summary instead of a wall of pruned placeholders — with a config
  (`auto_compact: bool`) to fall back to the old prune-only behavior for an
  embedder that would rather not spend an extra round-trip on every overflow.
- **Auto-summarize costs a paid round-trip on top of the turn's own request**,
  same cost profile as a manual `/compact`, just automatic. `EngineConfig`
  callers unaware of this feature get it by default; this is the intended
  behavior change the issue asks for, not an oversight.
- **Two different mutation semantics now share one wire variant**,
  distinguished only by `auto`. Kept to one variant rather than a new
  `OutEvent` because the payload (`summary`/`kept`) is identical; only what a
  subscriber *does* with it differs.
- **`Context::apply_compaction` is live code again**, not just a
  unit-tested-but-dead mechanism as ADR-0101/0102 left it.

## Rejected alternatives

- **Trigger policy: only after N consecutive prune-fallbacks, not every
  overflow.** Rejected for v1: needless state (a per-session counter) to save
  a round-trip in a case (repeated overflow within one session) that's
  already rare once summarization actually reclaims most of the budget every
  time. Simpler to always prefer summarization first; `auto_compact: bool`
  is the coarse off-switch if the extra round-trip is unwanted.
- **Copy-on-write auto path (fork mid-turn like `/compact`).** Rejected: a
  parked/streaming turn has no head-side actor available to receive a fork
  redirect, and the turn must still get an in-budget request to send *right
  now* — copy-on-write's entire premise (defer the decision to a
  user-observed report) doesn't apply mid-turn.
- **A distinct `OutEvent::AutoCompacted` variant instead of a shared `auto`
  flag.** Rejected: doubles the match arms in every subscriber
  (`replay`/`reducer`/`run`/`app`) for a payload that is otherwise identical;
  a bool discriminant reads just as clearly at each call site.
