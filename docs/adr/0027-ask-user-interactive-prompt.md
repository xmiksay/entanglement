# 0027. `ask_user` tool — model-driven user decision prompt

- Status: Accepted
- Date: 2026-07-09

## Context

The only user-facing prompt today is **binary tool approval**: on `Permission::Ask`
the runtime emits `OutEvent::ToolRequest` and parks for `InMsg::Approve` /
`InMsg::Reject` ([ADR-0014](0014-tool-approval-inline-modal.md)). The agent has
**no way to ask the user a question or offer a decision** — e.g. "which of these
approaches should I take?" — the way Claude Code's *AskUserQuestion* renders a
multiple-choice prompt. Users want that: model-initiated questions, with
multiple-choice options **plus** a free-text ("Other") escape.

## Decision

Add a **runtime-owned `ask_user` tool**, intercepted on `ToolExec` before
permission resolution — the same pattern as `spawn_agent` (ADR-0022) — so the
model triggers it by calling the tool and the round-trip reuses the existing
`ToolExec` → … → `ToolResult` machinery. Core stays unaware of the *interaction*:
it emits the `ToolExec` for `ask_user` like any other call and parks on the
`ToolResult`. The runtime executor (`ask_user.rs`) intercepts the call, drives
the head, and answers the parked turn.

- **Tool schema (model-facing):** `ask_user { question, options: [{label,
  description}], allow_free_form }` — advertised via `EngineConfig.tool_specs`
  (`ask_user::ask_user_spec()`, pushed alongside `spawn_agent`). Mirrors Claude's
  *AskUserQuestion* shape (labelled choices + optional free text). `options` is
  required; `allow_free_form` defaults to false but is forced true when `options`
  is empty (so there is always an answer path).
- **Runtime → head:** a **dedicated `OutEvent::UserQuestion { seq, request_id,
  question, options, allow_free_form }`**, plus a `WaitingApproval` status. The
  TUI enters a new interaction state (`PendingQuestion` in `session_view.rs`,
  distinct from `ApprovalMode`): the question + numbered choices render
  Claude-style, arrow/number keys select, an "Other" entry opens the shared input
  box for free text, `Esc` interrupts.
- **Head → runtime:** a **dedicated `InMsg::AnswerQuestion { request_id,
  answer }`**. Like `Approve`/`Reject`, the supervisor filters it out before
  routing (core never sees it) and the `ask_user` executor consumes it off the
  inbound fan-out (`Holly::subscribe_inbound`), then replies with a
  `ToolResult(answer)` the parent turn folds into `Context`.

### Resolved open questions

- **Dedicated events, not overloaded `ToolRequest`.** `UserQuestion` /
  `AnswerQuestion` are added purely so heads render multiple choice cleanly and
  future heads reuse them without conflating "approve this action" with "answer
  this question". They are protocol *types* only — the engine's turn loop gains
  no new logic (the supervisor drops `AnswerQuestion` off the fan-out exactly
  like `Approve`/`Reject`, #59).
- **Answer encoding.** The output fed back to the model is the answer text
  verbatim — the picked option's `label`, or the typed free-form string. No
  wrapper: the label *is* a meaningful answer, and a wrapper would only add noise.
- **Non-interactive heads.** `pipe` (raw NDJSON relay) already forwards
  `UserQuestion` and accepts `AnswerQuestion`, so a script answers normally. The
  one-shot `run` head has no user: it auto-answers with the first option's label
  (or `"(no interactive user available)"` when only free-form was offered) so the
  turn proceeds instead of parking forever.
- **`Stop` while pending.** Handled like approval: a `Stop` for the session
  unwinds the parked executor silently — core's `wait_tool_result` sees the same
  `Stop` and cancels the turn, so no `ToolResult` is owed.

Answer type: **both** — a fixed option list *and* a free-form escape.

## Consequences

- **Positive:** the agent can defer genuine decisions to the user; better
  interactive UX matching Claude Code; no core change.
- **Negative / neutral:** another special-cased runtime tool; a new TUI
  interaction mode to maintain; non-interactive heads need a defined fallback.

## Alternatives considered

- **Core protocol primitive** (`OutEvent::UserQuestion` + `InMsg::AnswerQuestion`
  parked in core like `ToolRequest`). First-class and head-agnostic, but adds
  core surface and policy for something the runtime can own via a tool — against
  the three-layer direction ([ADR-0006](0006-core-dependency-hygiene-gate.md)).
  A dedicated *OutEvent* may still be introduced purely for TUI rendering while
  keeping the trigger a runtime tool.
- **Overload tool approval** (`ToolRequest` with a magic tool name). Reuses the
  modal but conflates "approve this action" with "answer this question" and
  can't carry multiple choices cleanly.

## References

- Issue #90: `ask_user` interactive user-decision prompt
- [ADR-0014](0014-tool-approval-inline-modal.md): tool approval inline modal (the existing binary prompt)
- [ADR-0022](0022-subagent-spawn.md): runtime-owned tool intercept precedent
- [ADR-0006](0006-core-dependency-hygiene-gate.md): core dependency hygiene / three-layer split
