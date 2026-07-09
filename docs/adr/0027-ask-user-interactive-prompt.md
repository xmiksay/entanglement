# 0027. `ask_user` tool — model-driven user decision prompt

- Status: Proposed
- Date: 2026-07-09

> **Stub.** Captures the decision direction for issue #90. Flesh out
> (event/round-trip shape, TUI interaction mode, answer encoding) before
> implementation; promote to `Accepted` when the change lands.

## Context

The only user-facing prompt today is **binary tool approval**: on `Permission::Ask`
the runtime emits `OutEvent::ToolRequest` and parks for `InMsg::Approve` /
`InMsg::Reject` ([ADR-0014](0014-tool-approval-inline-modal.md)). The agent has
**no way to ask the user a question or offer a decision** — e.g. "which of these
approaches should I take?" — the way Claude Code's *AskUserQuestion* renders a
multiple-choice prompt. Users want that: model-initiated questions, with
multiple-choice options **plus** a free-text ("Other") escape.

## Decision (direction)

Add a **runtime-owned `ask_user` tool**, intercepted on `ToolExec` before
permission resolution — the same pattern as `spawn_agent` (ADR-0022) — so the
model triggers it by calling the tool and the round-trip reuses the existing
`ToolExec` → … → `ToolResult` machinery. No new *core* protocol semantics
required; core stays unaware of the interaction.

Sketch:

- **Tool schema (model-facing):** `ask_user { question, options: [{label,
  description}], allow_free_form: bool }` — advertised via
  `EngineConfig.tool_specs`. Mirrors Claude's shape (labelled choices + optional
  free text).
- **Runtime → TUI:** the executor surfaces the question to the head. Reuse
  `ToolRequest` semantics or add a dedicated `OutEvent::UserQuestion` (decide in
  fleshing-out — a dedicated event keeps the TUI rendering clean and is reusable
  by future heads without overloading approval). The TUI enters a new interaction
  mode alongside `ApprovalMode` (`session_view.rs`): render the question + choices
  Claude-style, arrow/enter to select, an "Other" entry that opens free-text
  input.
- **TUI → runtime:** the selected label (or typed free-form text) flows back;
  the executor returns it as the `ask_user` tool's `ToolOutput`, which the
  parent turn folds into `Context` like any tool result.

Answer type: **both** — a fixed option list *and* a free-form escape.

## Open questions (resolve before Accepted)

- Reuse `ToolRequest` vs. new `OutEvent::UserQuestion` + `InMsg::AnswerQuestion`.
  (Leaning: a dedicated event for clean rendering; weigh protocol surface.)
- How free-form vs. selected-option answers are distinguished in the output
  string fed back to the model.
- Behaviour of the stdio / non-interactive heads (auto-decline? first option?
  error?) so the tool degrades safely without a TUI.
- Interaction with `Stop` while a question is pending.

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
