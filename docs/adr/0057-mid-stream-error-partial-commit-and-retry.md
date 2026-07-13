# 0057. Mid-stream error: commit the partial, retry once before output

- Status: Accepted
- Date: 2026-07-13

## Context

A model reply is streamed: core emits an `OutEvent::TextDelta` for every text
chunk as it arrives, then commits the assembled assistant message to `Context`
via `push_assistant` after the stream ends cleanly. On a **mid-stream** error
(the stream yields an `Err` item after some deltas), the old turn loop skipped
`push_assistant` and jumped straight to `Error` + `Done`
([#181](https://github.com/xmiksay/entanglement/issues/181), part of the
engine-robustness epic #176).

Two problems followed:

- **Context diverged from the display.** The user already saw the partial text
  (the deltas were emitted), but core committed nothing. The next prompt
  therefore continued as if the assistant had said nothing — the conversation
  history no longer matched the transcript on screen.
- **No mid-stream recovery.** The provider retries connect-level failures and
  429s (keyed per endpoint, [ADR-0050](0050-per-endpoint-connection-pool-retry-rate-limit.md)),
  but once the stream has started yielding events those retries no longer apply,
  and the turn loop had no re-request of its own. A stream that dropped after the
  first byte failed the whole turn with no second attempt.

## Decision

Handle a mid-stream failure by the state of the *display*, and add exactly one
transparent re-request for the pre-output case (`session/turn.rs`):

- **Failure before any user-visible output** → re-request the same turn once
  (`STREAM_RETRIES = 1`). Nothing was shown, so a fresh stream is invisible to
  the user; the accumulators (`text_buf`, `tool_calls`, `finish`) are reset and
  the request is re-issued. This covers the "stream died at first byte" window
  the provider's own retry cannot. A second consecutive failure falls through to
  the error path.
- **Failure after a delta was shown** → **commit the partial** assistant message
  with an appended `\n\n[interrupted]` marker, then emit `Error` + `Done`. The
  marker is also streamed as a final `TextDelta`, so the display and the
  committed context end identically. The next prompt now sees an assistant turn
  that visibly ends in `[interrupted]`, matching the screen.

Any half-assembled tool calls are **dropped** on a mid-stream failure: without
the terminating `Finish` a tool call's arguments may be truncated, and executing
a partial call is unsafe. Only the partial text is committed.

This is purely additive on the wire — no new `OutEvent` variant. The marker is
ordinary assistant text carried on the existing `TextDelta`/`push_assistant`
path, so every head renders it with zero changes.

## Consequences

- Positive: the committed context can no longer diverge from what the user saw —
  the load-bearing fix. A follow-up prompt continues from the real (interrupted)
  transcript.
- Positive: a transient stream drop before output recovers silently within the
  same turn, with no `Error` surfaced and no user re-prompt.
- Positive: heads need no protocol change — the `[interrupted]` marker is plain
  streamed text.
- Neutral: the marker becomes part of the conversation the model sees next turn.
  That is the intent: it tells the model its previous reply was cut off.
- Negative: the pre-output retry is capped at one attempt and only fires while
  nothing has been shown; a stream that fails *after* output is not re-driven
  (that would require rolling back the display — see alternatives). Recovery
  there is a user re-prompt against the now-consistent context.

## Alternatives considered

- **Emit a distinct rollback event so heads undo the displayed partial, then
  re-request.** The issue floated this. It would allow a transparent retry even
  after output, but it adds a new `OutEvent` variant every head must learn to
  handle, and a head that missed it would leave stale text on screen — a worse
  divergence than the one being fixed. The marker keeps the fix inside the
  existing content stream with no new wire surface.
- **Commit the partial with no marker.** Aligns context with display but loses
  the signal that the reply was truncated; the model would treat a cut-off
  sentence as a complete thought. The marker costs a few tokens and makes the
  interruption explicit to both the user and the model.
- **Retry even after output (re-stream over shown text).** Produces duplicated
  or contradictory display text unless paired with a rollback event, so it
  reduces to the rejected first alternative.
- **Retry more than once.** A single pre-output re-request covers the transient
  first-byte drop without turning a persistent outage into a slow spin; repeated
  failures should surface to the user promptly, and the provider layer already
  owns backoff for the connect-level retries it does handle.
