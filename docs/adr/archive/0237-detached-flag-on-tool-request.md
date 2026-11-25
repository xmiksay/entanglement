# 0237. `ToolRequest` marks a card parked by detached work

- Status: Accepted
- Date: 2026-10-18
- Issue: #872 (found in the #849 review)
- Amends: [ADR-0230](0230-structured-card-reason-and-honest-scopes-on-tool-request.md)
  (`ToolRequest` and `PendingApproval` gain one more additive,
  serde-defaulted field) and
  [ADR-0002](0002-session-multiplexed-protocol.md)'s `OutEvent::ToolRequest`

## Context

A `background: true` `rhai` script
([ADR-0185](0185-rhai-joins-background-and-poll.md)) keeps running after the
turn that launched it ends. Its nested `Ask` parks an approval card in the
runtime's `PendingDecisions`, marked `detached` so a session end or hibernate
spares it (`stop_attached`,
[ADR-0232](0232-spawning-is-graded-bookkeeping-control-tools-non-maskable.md)).
An attended park has no timeout.

The TUI drops a view's whole approval queue on a terminal `Status`
(`Idle`/`Done`/`Error`): a finished turn's cards are void. The background
script's card was dropped with them. The runtime still waited on it, but
nothing showed it, so the script hung for good. The head could not tell the
two kinds of card apart: the wire had no fact for it.

## Decision

1. **`OutEvent::ToolRequest` gains `detached: bool`.** `true` for a card
   parked by detached work, i.e. a background script's nested call. It is
   serde-defaulted and omitted when `false`, so every turn-owned card, and
   every older log, keeps its exact wire shape.
2. **`PendingApproval` gains the same `detached`.** A reconnecting head
   rebuilds its view from `ListPending` and needs the same fact. The runtime
   builds the event and the registry entry from one `PendingApproval`
   (`pending::park_approval_as`), so the two can't drift, and
   `PendingDecisions::register_approval` reads the registry's own `detached`
   from it.
3. **A terminal `Status` drops only the turn's cards.** The TUI keeps cards
   with `detached: true` and promotes the first kept card to the prompt.
   A prompt already on a kept card (even mid reject-reason) stays.
4. **Every `Stop` clears the target's cards.** `stop_session` unwinds a
   background script's card too, so every TUI `Stop` site (normal and
   approval/question `Esc`, `/stop [--all]`, the sessions-modal `s`, the
   palette) goes through one helper, `stop_command::request_stop`, which
   sends the `Stop` and clears that session's whole queue.
5. **A view rebuilt from a log takes its detached cards from this
   process.** Resume rebuilds the view from the log. If this process
   already holds a view of that session (one it hibernated, whose card
   `stop_attached` spared, or a live one picked in the modal), that view is
   authoritative: its detached cards are carried over. A session this
   process never saw drops the log's detached cards, since its scripts
   died with the process that ran them.
6. **A moved prompt resets the approval UI.** When an event moves the
   active view's prompt off the card the user was on (a terminal `Status`
   dropped it and promoted a kept card), the pager closes, so its keys
   can't answer a card the user never read. A half-typed reject reason is
   cleared too, so it can't pre-fill the new card's reject. When no card is
   left, the reason stays as an ordinary draft.

Only a background `rhai` script parks detached cards. A background
`bash`/`call` job is approved once, at launch, and asks nothing after that.

## Consequences

- Approving a kept card resolves the script exactly as before: the runtime
  side is unchanged.
- `serve`/`pipe` relay the field verbatim; `skutter run` ignores it.
- An embedder head that clears cards on a terminal `Status` should keep the
  `detached` ones.
- A detached card whose park times out (an unattended mode) or is answered
  by another head stays shown until its session's next `Stop`: the wire has
  no "card gone" event. Follow-up: #883. The same gap has a narrow race: a
  detached `ToolRequest` the runtime emitted just before it handled a
  `Stop`, but that the TUI folds after its local clear, is shown again
  though `stop_session` already unwound it. No watermark guards this; #883
  covers it as a card resolved elsewhere.
- After a compaction the detached card stays on the closed predecessor's
  view: it is still answerable there, but a `Stop` on the successor neither
  unwinds nor clears it. #871 tracks moving it to the successor.
- A detached card parked after its session hibernated or ended carries seq
  0, because the session's counter was retired, so the TUI's seq dedupe
  drops it. Only cards parked before the hibernate survive it. Follow-up:
  #884.

## Rejected

- **Matching `request_id` against the turn's own calls.** A nested card's
  id is `<rhai id>:rhai:<tool>`, and a blocking script's nested cards *are*
  the turn's. The head would have to parse ids to guess.
- **Reusing `reason` (`CardReason`).** It explains what an approval honors,
  one value per card. A background script's floored write needs both facts.
- **Not clearing on a terminal `Status` at all.** The turn's own cards are
  void then, and the runtime unwinds them. Keeping them would leave stale
  prompts.
