# 0240. `ApprovalResolved` tells every head a card is gone

- Status: Accepted
- Date: 2026-10-19
- Issue: #883
- Amends: [ADR-0237](0237-detached-flag-on-tool-request.md) (its
  Consequences: an expired or elsewhere-answered detached card, and the
  `Stop` race) and [ADR-0238](0238-compaction-moves-a-background-scripts-card.md)
  (a stale copy a silent `Stop` heal leaves on a predecessor's view), and
  [ADR-0002](0002-session-multiplexed-protocol.md)'s `OutEvent` set

## Context

A head learns a card exists from `OutEvent::ToolRequest`, but the wire has no
fact for a card ending. Heads guess instead: a head drops a card it answered
itself, the TUI drops the turn's cards on a terminal `Status`, and every
`Stop` clears a session's cards locally.

The guesses miss three cases:

- With a mode `question_timeout`, `PendingDecisions::await_timed` forgets a
  card that timed out. A background script's card (ADR-0237) outlives its
  turn, so no `Status` follows. The TUI keeps it on screen until a `Stop`,
  and answering it does nothing.
- Two heads on one engine (TUI + `serve`, or two `serve` clients): a card
  one head answers stays on the other's screen. For a turn card this lasts
  until the turn ends. For a detached card it lasts until a `Stop`.
- ADR-0237's `Stop` race: a detached `ToolRequest` emitted just before the
  runtime handled a `Stop`, but folded after the TUI's local clear, shows
  again although `stop_session` already unwound it. ADR-0238's silent `Stop`
  heal has a related gap: it moves a card to the successor without an
  announce, then unwinds it, so the predecessor's view keeps its copy.

## Decision

1. **A new content event.** `OutEvent::ApprovalResolved { session, seq,
   request_id, outcome }`. `outcome` is an `ApprovalOutcome`:
   - `approved`: an `Approve`, from any head.
   - `rejected`: a `Reject`, from any head.
   - `expired`: the mode's `question_timeout` elapsed.
   - `stopped`: unwound with no decision. That covers a `Stop`, the
     session's end or hibernate (its turn's cards, `stop_attached`), a
     re-offer superseding a turn card (ADR-0238 §5), and any other frame
     aimed at the card's id (`AnswerQuestion`, `RetractQuestion`,
     `ReplaceQuestion`), which unwinds an approval park like a `Stop`. That
     unwind is a pre-existing bug, which wedges the turn (#895). Heads now at
     least drop the card.

   The outcome is what the waiter actually got. A decision that reaches a
   waiter that already gave up (its timeout fired, but `forget` has not run
   yet) reports `expired`, not the decision. An unwind keeps `stopped` for a
   gone waiter: `stop_session` aborts the session's tasks before it unwinds
   their cards, so a closed waiter there is usually an aborted one. A session
   end or hibernate (`stop_attached`) aborts nothing first, so there a closed
   waiter is a timeout caught in a narrow window; `stopped` is still a
   truthful end for a head.

   A superseded card is `stopped`, not a fifth outcome: its waiter gets
   `Stop`, and a head only needs to drop it.
2. **Approvals only.** The event covers every `ToolRequest` card (turn and
   detached alike) and no `ask_user` question. A question is always turn-bound:
   a script cannot call `ask_user`, so no question is ever detached, and
   the turn's terminal `Status` clears it on every head. A question another
   head answered, or one that timed out, is stale for the rest of that turn at
   most, never indefinitely. ADR-0146's retract/replace are head-to-engine
   frames with their own semantics. Folding questions in would mean a second
   card model in one event for a gap that ends on its own. The remaining
   within-turn staleness is #894.
3. **The registry emits it whenever a card leaves.** `PendingDecisions`
   emits it for every entry that carries a wire approval (the `ToolRequest`
   it announced), at each place an entry is removed:
   - `resolve`;
   - `forget` (a timeout's cleanup in `await_timed`);
   - `stop_session` and `stop_attached`;
   - `insert`'s supersede of a turn card.

   That is every removal path. The executor builds the registry with
   `PendingDecisions::announcing(holly)`. `PendingDecisions::default()`
   (tests, an embedder that reports on its own) emits nothing.
4. **Under the registry lock, after the card.** Every emit runs while the
   lock is held. The park's `ToolRequest` (`register_parked`'s `emit`) and a
   move's announce (`reassign_detached`'s `emit`) are emitted under the
   same lock (ADR-0238 §6). So:
   - the end of a card always follows its `ToolRequest`;
   - a supersede's `stopped` precedes the new card's `ToolRequest`, so a
     head that drops by id keeps the new card;
   - the event is emitted exactly once, because it is tied to the map
     removal: a decision racing a timeout removes the entry once, and the
     loser finds nothing.

   `emit_for_session` only takes the seq registry's lock and broadcasts, so
   it cannot re-enter the registry.
5. **On the card's home.** The event names `Parked.home`, the session the
   card was last shown on (ADR-0238). A head that keeps per-session copies
   drops the id along that session's compaction predecessors too. That
   covers the silent `Stop` heal and is the scope ADR-0238 §7 already uses
   for a moved card.
6. **Persisted and seq-gated.** It is a content event with a fresh
   per-session seq, so the persistence tap logs it. Core replay ignores it,
   since it has no effect on `Context`. A view rebuilt from the log (the
   TUI's `restore_from_records`, `ReplayFrom`'s `History`) folds it and
   drops the card, so a resumed head does not show a card already resolved.
   `History` only returns records past `after_seq`, so it never returns a
   seq-0 end; a `ReplayFrom` rebuild keeps such a card. ADR-0239 gives a
   retired session real seqs, so that is left to #891's eviction and to logs
   written before it.
   `ListPending` already omits removed cards. A head folds it only past its
   seq watermark, like the card itself, so a stale end cannot drop a newer
   card that reuses the id (Gemini's `name#ordinal` ids repeat every round).
   The exception is seq 0, minted for a session with no sequence: one that
   never started, one whose high-water was evicted (#891), or, in a log
   written before ADR-0239, a retired one. A seq-0 end is folded by id
   without moving the watermark, like the reducer's seq-0 `Error`. That is safe: a seq-0 card is dropped on
   arrival, so no newer card can hold the id.
7. **Wire and hooks.** It is an `OutEvent`, so heads only receive it.
   `serve` and `pipe` relay it verbatim, `skutter run` prints nothing for
   it, and `InMsg::wire_allowed` is untouched. No hook fires on it:
   `Notification` means "a human is needed", and a card's end needs nobody.
8. **The TUI.** The reducer's seq-gated fold drops the id from the view's
   queue (`drop_approval`), which re-derives the prompt. Attention is read
   off the queues, so it follows. `SessionRegistry` drops the id along the
   predecessor chain. When the dropped card was prompted, `App`'s
   `prompted()` comparison applies ADR-0237 §6: the pager closes, and a
   half-typed reject reason is taken when another card takes the prompt.

   The local clears stay as belt and braces:
   - `request_stop` clears at once, so the screen does not wait on a round
     trip.
   - `drop_turn_approvals` on a terminal `Status` covers an older runtime.
   - ADR-0238's drop along predecessors on a detached `ToolRequest` is a
     move, not an end, so this event does not cover it.

   Each is idempotent with this event.

## Consequences

- Every head can drop a card by id when it ends, whatever ended it. ADR-0237's
  expired and elsewhere-answered detached cards and its `Stop` race are
  closed, and so is the copy a silent `Stop` heal left on a predecessor's view.
- Each resolved card consumes one seq.
- An end can come after its session's counter is retired: no terminal
  `Status` precedes `SessionEnded`/`SessionHibernated`. Two cases hit it:
  - (a) A session closed (`CloseSession`, wire-allowed) or hibernated while
    parked on a turn card. `stop_attached`'s `stopped` goes out after the
    retire, and nothing else clears that card.
  - (b) The #883 case itself: a detached card that an idle-TTL hibernate
    spared, which later expires or is answered by another head.

  ADR-0239 (#884) mints real seqs for a retired session, so both fold past
  the watermark. #891 (an evicted high-water entry falls back to seq 0) and
  logs written before ADR-0239 can still produce a seq-0 end; a head that
  dropped it would keep the card forever, so a seq-0 end is folded by id
  without moving the watermark (§6). That covers a live head and the TUI
  restore, but not a `ReplayFrom` rebuild, whose `History` filters seq-0
  records (§6).
- An embedder that parks through `register_approval` and emits its own
  `ToolRequest` must do so straight after registering (register-then-emit,
  #156). Otherwise a `Stop` or timeout in between reports an end before the card.
- An older head built against the previous `OutEvent` fails to parse the new
  `kind`, because serde's internally tagged enums have no catch-all. A wire
  consumer should read the `kind` tag first and skip kinds it does not
  know. An older `skutter` cannot load a log that holds the event, as with
  every added variant.

## Alternatives considered

- **Emit it only for detached cards.** A head would need two rules, and the
  two-head gap on turn cards would stay open.
- **Live-only (no seq, not persisted).** A resumed view would show resolved
  cards again, and without a seq a stale end could drop a newer card with
  the same id.
- **Emit from the decision router.** The router sees decisions and `Stop`s,
  but not a timeout or `stop_attached`. Its emit would also not be tied to
  the map removal, so the order against the card and exactly-once would
  not hold.
- **Emit outside the lock.** A move or a re-park could run between the
  removal and the event, so the end could follow a newer card with the same
  id, or name a home the card no longer has.
- **Emit on the key session as well as the home.** After a compaction the
  key session is ended, and every head would get two events for one end.
- **Cover `ask_user` questions too** (§2). It would add a second card model
  to the event for a gap that ends with the turn.
- **Heads poll `ListPending`.** That costs a round trip per head and still
  races, because a card can end between the listing and the next one.
