# 0245. A decision only answers its own kind of card

- Status: Accepted
- Date: 2026-10-20
- Issues: #895, #894
- Amends: [ADR-0240](0240-approval-resolved-event.md) §1 (a non-approval
  frame aimed at an approval card's id is no longer `stopped`) and §2 (the
  within-turn question staleness it left to #894), and
  [ADR-0146](0146-ask-user-list-retract-replace.md)'s Consequences (every
  approval park folded `Retract`/`Replace` into its unwind arm)

## Context

`PendingDecisions` holds two kinds of park under one map: `ask_user`
questions, answered by `AnswerQuestion`/`RetractQuestion`/`ReplaceQuestion`,
and approval cards (permission `Ask`, `propose_plan`, `request_mode`, a
`rhai` card), answered by `Approve`/`Reject`. `resolve` delivered any
decision to whatever park held the id, and each park folded the other
kind's decisions into its `Stop` arm.

A `Stop` unwinds silently because core cancels the turn on the same frame,
so no `ToolResult` is owed. A misaimed decision cancels nothing. A wire
`RetractQuestion` aimed at a turn card unwound the park, no `ToolResult`
followed, and the turn waited forever (#895). Since ADR-0240 every head
also dropped the card on its `stopped` end, so the wedge was invisible. An
`Approve` aimed at a question wedged the turn the same way.

## Decision

1. **The registry matches a decision to its kind of park.**
   `PendingDecisions::resolve` returns a `Resolution`: `Delivered`, `Absent`
   (no card holds the id, a no-op as before), or `Misaimed { kind }`. A
   question decision only reaches an `ask_user` park and an approval
   decision only an approval park. A misaimed decision leaves a live card
   parked and reports no end, because nothing ended. A card whose waiter
   already gave up is taken like any late decision and reports `expired`.
2. **The router refuses it.** On `Misaimed` the decision router emits
   `OutEvent::Error` on the session the frame addressed, naming the frame,
   the id and the frames that answer it. The card stays listed
   (`ListPending`/`ListQuestions`) and answerable.
3. **Parks see only their own decisions.** Approval parks match
   `seam::ApprovalDecision` (`Approve`/`Reject`/`Stop`) and the `ask_user`
   park `seam::QuestionDecision` (`Answer`/`Retract`/`Replace`/`Refused`/
   `Stop`), via `Decision::into_approval`/`into_question`. The per-park
   "treat as `Stop`" arms are gone. `Refused` is the registry refusing the
   question's own park (a live card already holds the id on the session,
   ADR-0238). The call replies with the reason as an error result, since
   the turn keeps running. `ask_user` lists and shows a question only while
   its fresh waiter is still undecided (`oneshot::Receiver::is_empty`,
   which never consumes the value), so a refused park, or a `Stop` or
   decision that raced in, shows nothing and the await takes it. A refusal
   on a replace's re-park clears the question the earlier pass listed and
   moves the status off `WaitingAnswer`.
4. **A question frame answers no moved card.** The router's heal before a
   decision (ADR-0238) skips announcing the card the frame answers. Only
   approval cards move, so a question frame now announces every card it
   moves: the refused card stays parked and must be shown.
5. **Questions end on their call's `ToolOutput`** (#894, no protocol
   change). Every terminal `ask_user` outcome but a `Stop` replies: an
   answer from any head, a retract, a `question_timeout` default or error,
   a refused park's error.
   Core emits the call's `ToolOutput` as soon as that result arrives, so a
   head drops the question with that `request_id` there. A
   `ReplaceQuestion` re-emits `UserQuestion` under the same id and sends no
   result; a head replaces the queued entry in place. A question is
   turn-bound and never moves, so its result lands on the view that shows
   it. A `Stop` still clears on the turn's terminal `Status`.

## Consequences

- A misaimed frame no longer wedges a turn, and heads no longer drop a card
  that is still waiting. Its sender sees an `Error`; every other head
  subscribed to the session sees it too, as with any session `Error`.
- The refusal is an ordinary session `Error`, and `Error` carries no
  field to tell it from a turn error, so the runtime's in-process watchers
  read it like one. That is not new: core already emits session errors
  outside any turn's failure (a refused `SetModel` in `session/commands.rs`,
  a failed revert in `revert.rs`, a refused fork in `user_fork.rs`), and
  the same watchers read those.
  Accepted rather than adding a protocol field for a client bug's frame;
  each is still better than the wedged turn it replaces:
  - the hooks' lifecycle tap records it as the session's `last_error`, so
    the next turn-end hook (`stop`/`subagent_stop`) names the refusal as
    the turn's error;
  - the session-title generator's wait treats it as the turn settling and
    fires its aux call early;
  - a parent waiting on a sub-agent keeps it as the turn's error: if that
    child turn then ends with no text, the parent is told the child "ended
    its turn in error" (naming the refusal) and keeps waiting for a steer,
    instead of returning "produced no output".
- ADR-0240's `stopped` covers a `Stop`, a session end or hibernate, and a
  superseding re-offer. A misaimed frame produces no end at all.
- `PendingDecisions::resolve` now returns `Resolution`; an embedder routing
  its own decisions should surface `Misaimed`.
- ADR-0240 §2's reason to leave questions out stands: a question has its
  own end on the wire, the `ToolOutput`, so it needs no `ApprovalResolved`.

## Alternatives considered

- **Unwind with a `ToolResult`.** A misaimed `RetractQuestion` could reply
  "withdrawn" for an approval card. It would treat a client bug as the
  user's decision on a card they may never have seen.
- **Refuse in every park.** Each park would need the router's channel to
  report the refusal and a way to re-park. One check in the registry needs
  neither.
- **Silently drop the misaimed frame.** It keeps the card but leaves the
  sender guessing why nothing happened.
- **Extend `ApprovalResolved` to questions** (#894's alternative). The
  `ToolOutput` already marks a question's end on every head; a second event
  would carry the same fact.
