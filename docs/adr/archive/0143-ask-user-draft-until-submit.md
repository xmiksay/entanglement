# 0143. `ask_user` answers are drafts until an explicit Submit

- Status: Accepted
- Date: 2026-07-31
- Amends: [0127](0127-ask-user-v2-multi-question-envelope.md)

## Context

[#518](https://github.com/xmiksay/entanglement/issues/518): ADR-0127 gave
`ask_user` a multi-question envelope, but each question's `Enter`/number-pick
committed *and sent* immediately once it was the last question in the batch —
there was no way to change an answer already given short of stopping the whole
turn and re-prompting. The Claude Code / opencode convention (referenced by
the issue as the mechanism to adopt) treats every answer as a draft until one
explicit Submit step, walkable back and forth in the meantime.

## Decision

Keep ADR-0127's wire contract untouched — still one `OutEvent::UserQuestion`
per call, one `InMsg::AnswerQuestion` resolves it — and change only the head
side's walk of the batch:

- **`PendingQuestion.answers` becomes `Vec<Option<Vec<String>>>`**, parallel to
  `questions`, `None` until that question has a draft. Committing a question
  (`Enter`/number-pick) writes `answers[current]` **in place** rather than
  pushing — so stepping back and recommitting overwrites the old draft instead
  of appending a duplicate.
- **`current == questions.len()` is a new terminal review/submit step.** Once
  every question has a draft, the call parks there instead of firing
  `AnswerQuestion` on the last commit. `question.rs` renders it as a summary —
  each question paired with its drafted answer — whose own `Enter` is the one
  explicit Submit that sends the batch; `Esc`/`←`/`Backspace` there steps back
  to the last question to revise instead, sending nothing and leaving the call
  parked (satisfies the "Esc before Submit sends nothing, keeps it parked"
  acceptance bar — distinct from a mid-question `Esc`, which still interrupts
  the turn exactly as before, unchanged by this issue).
- **`Left`/`Backspace` step back one question** (a no-op on the first), only
  active outside free-text entry (where they already mean cursor movement).
  `PendingQuestion::sync_from_draft` restores the on-screen selection (checked
  boxes / highlighted option) from that question's existing draft — including
  reloading free-form text into the shared input box when the draft wasn't
  among the offered options — so a revisited question shows its answer instead
  of resetting blank.
- **A single-question call gets the same review step.** Consistency was an
  explicit acceptance criterion: picking the one option no longer sends
  anything by itself, it lands on the one-question review screen and still
  needs the explicit Submit.
- **No protocol change.** `AnswerQuestion`'s `answers: Vec<Vec<String>>` is
  unwrapped from the drafts (`PendingQuestion::answers_for_submit`, `None`
  unless every entry is `Some`) only at the moment Submit is pressed — the
  runtime's `PendingDecisions` waiter (`pending.rs`) is still consumed exactly
  once, unaware any of this happened.

## Consequences

- **Positive:** a user can correct an earlier answer in the same batch without
  aborting the turn; single- and multi-question calls behave consistently; no
  wire/protocol/runtime change, so the `ask_user` tool and its tests are
  untouched.
- **Negative / neutral:** one extra keypress to leave the review step even for
  a single-question call (a deliberate trade against the previous
  auto-send-on-last-answer instant confirmation); `PendingQuestion` carries a
  little more state (`Option` drafts + the review step) than the plain
  sequential walk it replaces.

## Alternatives considered

- **Auto-submit on the last question, add an "undo last answer" key instead.**
  Rejected: only lets you revise the *most recent* answer, not any earlier one
  in the batch, and still needs a wire round-trip once the model has already
  seen the (wrong) tool result if the undo comes after the batch was sent.
- **Skip the review step for a single-question call** (send immediately like
  before, only batches of 2+ get drafts). Rejected by the issue's own
  acceptance criteria — a picked option not being auto-sent is explicitly
  called out as required for consistency between the two shapes.
- **Post-submit revision via a rewind/fork.** Out of scope for this issue (see
  the issue body): after Submit, the answer is folded into the turn and
  becomes part of the append-only context (ADR-0061); revising it is
  history-rewriting, for which the pragmatic recourse today is a corrective
  mid-turn `Prompt` (folds into the live turn, #182/[ADR-0058](0058-mid-turn-prompt-folds-into-live-turn.md)).
  A true rewind (fork a successor truncated before the `AnswerQuestion`, the
  ADR-0101 copy-on-write machinery) is deliberately not part of this issue.

## References

- Issue #518: TUI draft-until-submit for `ask_user` answers
- [ADR-0127](0127-ask-user-v2-multi-question-envelope.md): the v2 envelope this
  amends — wire contract unchanged
- [ADR-0058](0058-mid-turn-prompt-folds-into-live-turn.md): the corrective
  mid-turn `Prompt` recourse for post-submit revision (out of scope here)
- [ADR-0061](0061-parked-turn-state-batch-tool-resolution.md): the parked-turn
  contract `PendingDecisions` still fulfils exactly once, at Submit
- Related: #512, #515 (open-question introspection over the wire, tracked
  separately)
