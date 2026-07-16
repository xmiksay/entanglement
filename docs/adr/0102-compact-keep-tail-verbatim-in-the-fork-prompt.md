# 0102. `/compact` keep-tail: a verbatim tail composed into the fork prompt

- Status: Accepted
- Supersedes the "Keep-tail under copy-on-write" rejected alternative in
  [0101](0101-compaction-forks-into-a-new-session-copy-on-write.md) (the
  reasoning there — "there is no tail to keep, the source retains the whole
  history" — is about the *source*; this ADR is about the *fork*'s fidelity)
- Date: 2026-07-16
- Issue #397 (part of #396)

## Context

ADR-0082 shipped `/compact` with `kept` always `0`, deferring keep-tail to v1
with a concrete blocker: a `Tool`-role message replayed without its
immediately preceding `Assistant` parent (the one that issued the tool call)
breaks providers' `tool_use`/`tool_result` block-pairing requirement.
Turn-boundary detection — splitting only at a point where every tool
round-trip is intact — wasn't built.

ADR-0101 then made `/compact` copy-on-write: the source session's `Context` is
never mutated, so the *source* never loses anything regardless of `kept`.
ADR-0101's "Rejected alternatives" dismissed keep-tail on that basis ("there is
no tail to keep — the source retains the whole history"). That's true for the
source, but it left a real gap in the **fork**: the new session's only seed is
the LLM's summary. A long conversation's most recent turns — the ones most
load-bearing for continuing the work — get paraphrased through the summarizer
along with everything else, discarding exact code, file paths, and tool output
detail that a "kept" tail existed to preserve in the first place.

## Decision

`/compact`'s `args.kept: u64` (optional, default `0`) becomes a real
keep-tail request, honored **without any wire/protocol change**:

1. **Turn-boundary detection lives on `Context`.** `Context::safe_kept(&self,
   requested_kept: usize) -> usize` clamps a requested count to the nearest
   safe boundary: the tail must start at a `User`-role message (turns begin on
   a user prompt), so every `Assistant`/`Tool` round-trip in the turn before it
   stays fully inside the summarized head. It walks forward from the naive
   split point to the next `User` message — fewer kept messages over an unsafe
   boundary — collapsing to `0` if no later `User` message exists.
   `Context::apply_compaction` (dead on the live path since ADR-0101, kept for
   its own unit tests and as a public mechanism) calls it internally too, so
   the invariant holds for any caller.

2. **`compact_op` summarizes only the head.** `s.ctx.messages()` splits at
   `len - safe_kept(requested_kept)` into `head`/`tail`. Only `head` is
   rendered into the summarization prompt — the model never sees (and can't
   accidentally paraphrase) the kept tail. `head` empty (an over-large `kept`
   swallowing the whole conversation) is a recoverable `Error`, not a
   degenerate zero-content LLM call.

3. **The tail rides verbatim inside `summary`.** After a successful,
   non-truncated summary, `compact_op` appends the tail's rendered transcript
   (the same `[role]\n...` framing `render_transcript` already produces for
   the summarization prompt) after the LLM summary, clearly delimited. The
   composed text is what ships as `OutEvent::Compacted.summary` — exactly the
   field the TUI already forks verbatim into the new session's first message
   (`wrap_compaction_summary`/`InMsg::Spawn { prompt, .. }`). **No head/TUI
   change is required** for the tail to land in the fork.

4. **`kept` on the wire now reports the real (clamped) count**, not a
   hardcoded `0` — informational metadata for the head/user, not itself load-
   bearing (the tail's content already rode inside `summary`).

5. **A guard against a tail that alone overflows the budget.** The kept
   tail is unsummarized, so if its rendered size alone exceeds the source
   session's context budget, forking would start the new session already over
   its window. `compact_op` rejects this case before calling the LLM — same
   posture as the existing oversized-head-transcript guard.

6. **TUI**: `/compact [--keep N] [instructions]` (`tui::commands::
   parse_compact_args`, the same raw-text re-parse pattern as `/set`/`/mcp`).
   The command-palette pick still sends `kept: 0` (no trailing text to parse,
   same limitation `/set` already has).

## Consequences

- **No protocol change.** `InMsg::Oneshot`'s `args` shape and
  `OutEvent::Compacted { summary, kept }` are unchanged; older records
  (`kept: 0`, no verbatim tail appended) still replay/render correctly — `kept`
  is `#[serde(default)]`.
- **The fork keeps full fidelity on its most recent turns** while everything
  before them is compressed — the actual value keep-tail always promised,
  delivered through the fork instead of an in-place mutation.
- **A "verbatim tail" here means a text rendering, not structurally preserved
  `Message` objects.** The new session's first message is a single `User`-role
  turn (summary + tail text) — the model sees the recent turns' exact content,
  but not as separate role-tagged messages the way the source session had
  them. This trades some role fidelity for zero protocol/wire churn; if a
  future need requires literal message-object preservation in the fork, that's
  a bigger change (seeding `Spawn` with more than a single `prompt: String`)
  deliberately left out of scope here.
- **Turn-boundary detection is conservative.** It only recognizes a `User`
  message as a safe split point, per ADR-0082's simplest-correct-rule framing;
  an `Assistant`-only reply (no tool call) just before a safe boundary gets
  folded into the head/summary rather than kept, even though keeping it alone
  wouldn't violate any pairing constraint. Simpler and provably safe beats
  marginally tighter.

## Rejected alternatives

- **Extend `OutEvent::Compacted`/`InMsg::Spawn` to carry structured
  `Vec<Message>`.** Would preserve tail messages as literal role-tagged
  provider messages in the fork instead of one flattened text block — strictly
  more faithful, but touches the wire (`Compacted` gains a field, `Spawn` gains
  an optional seed-messages field used by exactly one caller), persistence, and
  every `Spawn` call site (the sub-agent `agent_spawn` tool has nothing to do
  with compaction). Deferred: the flattened-text approach delivers the actual
  user value (recent detail survives compaction) at a fraction of the surface
  area, and nothing about it forecloses a structured version later.
- **Have the TUI reconstruct the tail from its own transcript view** (the
  `SessionView` the TUI already renders from `OutEvent`s) instead of core
  composing it. Rejected: the TUI's transcript is a *display* projection, not
  guaranteed to line up 1:1 with `Context::messages()` (e.g. streamed deltas
  vs. one `Message`), and core already holds the authoritative history — no
  reason to trust a head-side reconstruction over the source of truth.
