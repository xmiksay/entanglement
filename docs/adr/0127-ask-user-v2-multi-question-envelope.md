# 0127. `ask_user` v2 — multi-question envelope, always-on free text, multi-select

- Status: Accepted
- Date: 2026-07-22
- Amends: [0027](0027-ask-user-interactive-prompt.md)

## Context

[#488](https://github.com/xmiksay/entanglement/issues/488): ADR-0027's
`ask_user` supports exactly one question per call, single-select only, and
free text only when the model explicitly sets `allow_free_form`. Three gaps
in practice:

- A model that needs to ask several related questions (which database, which
  regions, which auth method) had to call `ask_user` repeatedly — one
  `ToolExec`/`ToolResult` round-trip and one `WaitingAnswer` park per question,
  instead of batching them.
- `allow_free_form` defaulting to `false` meant a user who wanted to type a
  different answer than any offered option had no escape unless the model
  had anticipated it.
- No way to let the user pick more than one option for a single question
  (e.g. "which regions should this deploy to?").

## Decision

Keep ADR-0027's core shape — a runtime-owned tool intercepted before
permission resolution, one `OutEvent::UserQuestion` parks the turn, one
`InMsg::AnswerQuestion` resolves it, folded back as a single `ToolResult` — and
widen the payload:

- **`questions: Vec<Question>`, not one question.** `ask_user`'s schema becomes
  `{"questions": [{question, options: [{label, description?}], multi_select?}]}`
  (1..N). One `ask_user` call → one `OutEvent::UserQuestion` carrying the whole
  array → the head walks it and collects every answer → one
  `InMsg::AnswerQuestion` → one folded `ToolResult`. This keeps the
  one-`ToolExec`/one-`ToolResult` contract intact (core stays unaware, per
  ADR-0027): no sub-request-id plumbing, no change to how the turn loop parks,
  atomic replay of the whole exchange.
- **Free text is unconditional.** `allow_free_form` is dropped from the wire
  contract; every question always offers a typed "Other" answer. There is
  nothing left to opt into, so the flag just disappears rather than defaulting
  to `true`.
- **`multi_select` is per-question**, not per-call: a batched call can mix a
  single-select "which database" with a multi-select "which regions" in the
  same round-trip.
- **`InMsg::AnswerQuestion.answers: Vec<Vec<String>>`** — one inner vec per
  question, in the call's `questions` order; a multi-select's picks or a
  single-select's one pick or free text are all just strings in that inner
  vec. The runtime folds them back as one line of tool output per question
  (`question: chosen, labels`), or the bare answer when only one question was
  asked (unchanged from v1's single-line output).
- **Wire evolution, not a breaking bump** (mirrors [ADR-0064](0064-message-content-blocks.md)'s
  content-block migration): `OutEvent::UserQuestion.questions` flattens onto
  the wire (`#[serde(flatten)]` over an untagged `Questions` newtype) so a
  legacy single-question log (`question`/`options`/`allow_free_form` as
  sibling top-level keys) still deserializes into a one-element vec, with
  `allow_free_form` read and discarded. `InMsg::AnswerQuestion` keeps the
  legacy `answer: String` field alongside the new `answers`, defaulted empty on
  both sides; a legacy frame's `answer` folds to `[[answer]]` in
  `seam::Decision::from_inmsg` — the one place that mapping happens. Neither
  field is written by a current head.
- **TUI walks the batch sequentially.** `PendingQuestion` now represents one
  *call* (all its `questions`, a `current` index, buffered `answers`, and the
  current question's checked-option set) rather than one question; the
  existing `pending_questions: VecDeque` queue (batched tool calls, #273) is
  unchanged — it still queues *calls*, each of which now walks internally
  before popping. `question.rs` renders checkboxes for a multi-select question,
  a single highlighted marker otherwise, an unconditional "Other" row, and a
  "(2/3)" progress suffix when the call has more than one question. `Space`
  and number keys toggle a multi-select's checkboxes; `Enter` submits the
  current question (immediately for single-select, the checked set for
  multi-select) and only sends `AnswerQuestion` once the last question in the
  call is answered.

## Consequences

- **Positive:** a model can batch related questions into one interaction
  instead of serial round-trips; the user always has a typed escape; the user
  can multi-select where it makes sense. No core change, no new wire variant.
- **Negative / neutral:** `PendingQuestion` and the question-rendering code
  are more stateful (walking a batch + buffering answers vs. one question one
  answer); the legacy-shape deserializer is an extra serde indirection
  (`Questions` newtype) that field-level `deserialize_with` alone can't express,
  since it must fold *sibling* top-level keys into one field, not just
  reinterpret one field's own shape.

## Alternatives considered

- **N events, one per question, still one call.** Emit `UserQuestion` once per
  question and let the head reply to each with its own `AnswerQuestion`,
  folding only the last into the `ToolResult`. Rejected: reintroduces
  sub-request-id plumbing (which `AnswerQuestion` belongs to which pending
  question within the call) that the one-event envelope avoids entirely, and
  complicates replay — a mid-batch resume would need to reconstruct partial
  answer state instead of replaying one `UserQuestion`/`AnswerQuestion` pair.
- **Keep `allow_free_form` as an opt-out.** Considered leaving the flag and
  just defaulting it to `true`. Rejected: a flag with one live value is dead
  weight — every head would still branch on it for no behavioral difference,
  and a future model integration would have to learn what the flag *used* to
  mean from old logs.
- **`multi_select` per-call instead of per-question.** Simpler schema, but
  forces every question in a batch to share one selection mode even when a
  batch naturally mixes single- and multi-choice questions.

## References

- Issue #488: `ask_user` v2 — multiple questions per call, always-available
  custom answer, multi-select
- [ADR-0027](0027-ask-user-interactive-prompt.md): the v1
  `ask_user` tool this amends
- [ADR-0064](0064-message-content-blocks.md): the wire-evolution pattern this
  follows (field shape migrates with a serde back-compat shim, not a version
  bump)
- [#273](https://github.com/xmiksay/entanglement/issues/273): the pending-queue
  batching (`pending_questions`/`pending_tool_requests`) this reuses unchanged
