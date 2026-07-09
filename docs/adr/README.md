# Architecture Decision Records

An **ADR** is a short, immutable record of *why* an architecture decision was
made. The [architecture doc](../architecture.md) describes the current state
(*what is*); ADRs are the decision log (*how we got here, and what else we
considered*). The two run in parallel: a decision lands here first, then the
arch doc is updated to reflect it.

## When to write one

Write an ADR for any decision that is **hard to reverse** or that a reader would
reasonably ask *"why?"* about — protocol shapes, crate boundaries, a chosen
pattern over an obvious alternative, security/permission models. Don't write one
for trivial refactors or local naming.

## Format

File name: `NNNN-kebab-case-title.md` (zero-padded, monotonically numbered).
Each record:

```
# NNNN. Title
- Status: Proposed | Accepted | Superseded by ADR-XXXX | Deprecated
- Date: YYYY-MM-DD

## Context
Why this came up — forces, constraints, what the reference projects do.

## Decision
What we chose, precisely.

## Consequences
Positive / negative / neutral effects.

## Alternatives considered
The options rejected and why. (This is the part the arch doc can't carry.)
```

## Status lifecycle

`Proposed` → `Accepted` → (`Superseded by ADR-XXXX` | `Deprecated`). Never edit
an accepted ADR in place — supersede it with a new one that links back.

## Index

| # | Title | Status |
| --- | --- | --- |
| [0001](0001-actor-model-abi.md) | Actor model is the integration ABI | Accepted |
| [0002](0002-session-multiplexed-protocol.md) | Session-multiplexed wire protocol | Accepted |
| [0003](0003-agent-and-permission-profiles.md) | Agent + permission profiles (opencode-style) | Accepted |
| [0004](0004-structured-plan-and-task-events.md) | Structured Plan & TaskList events (profiles + events, both) | Accepted |
| [0005](0005-ndjson-stdio-head.md) | NDJSON stdio head (`run` + `pipe`) | Accepted |
| [0006](0006-core-dependency-hygiene-gate.md) | Layering: core / provider / runtime + core hygiene gate | Accepted |
| [0007](0007-streaming-llm-and-provider-crate.md) | `entanglement-provider`: streaming `Llm` trait, pooling, retry, rate-limit, reasoning | Accepted |
| [0008](0008-host-tools-workdir-and-bounded-output.md) | Host tools: working-directory root + bounded output | Accepted |
| [0009](0009-edit-and-bash-host-tools.md) | Host tools: `edit` (search/replace) and `bash` (subprocess + timeout) | Accepted |
| [0010](0010-single-head-crate-and-bash-opt-in.md) | `entanglement-runtime`: the head crate — tools, execution, permissions, sessions | Accepted |
| [0011](0011-tui-head-ratatui-crossterm.md) | TUI head: ratatui + crossterm in `entanglement-runtime` | Accepted |
| [0012](0012-tui-event-buffering-rendering.md) | TUI event-buffering & rendering model | Accepted |
| [0013](0013-keybinding-leader-which-key.md) | Keybinding scheme: leader key + which-key | Accepted |
| [0014](0014-tool-approval-inline-modal.md) | Tool approval UX: inline card vs modal | Accepted |
| [0015](0015-rich-text-pipeline-syntect.md) | Rich-text pipeline: pulldown-cmark → ratatui Text, syntect for code blocks | Accepted |
| [0016](0016-host-tool-empty-result-contract.md) | Host tools: empty-result contract (no silent zero-output) | Accepted |
| [0017](0017-stop-cancels-turn-not-session.md) | `InMsg::Stop` cancels the turn, not the session | Accepted |
| [0018](0018-turn-loop-stash-discipline.md) | Turn-loop command stash discipline | Accepted |
| 0019 | — _(number skipped; no ADR-0019)_ | — |
| [0020](0020-event-sourced-session-persistence.md) | Event-sourced session persistence | Accepted |
| [0021](0021-hierarchical-session-model.md) | Hierarchical session data model | Accepted |
| [0022](0022-subagent-spawn.md) | Sub-agent spawn and parent→child answer relay | Accepted |
| [0023](0023-subagent-spawn-limits.md) | Sub-agent spawn recursion / fan-out limits | Accepted |
| [0024](0024-subagent-permission-gating.md) | Sub-agent spawn permission gating and privilege ceiling | Accepted |
| [0025](0025-runtime-cargo-feature-gates.md) | `entanglement-runtime` cargo feature gates (`cli`/`tui`) for lean library embedding | Accepted |
| [0026](0026-async-subagent-spawn-and-poll.md) | Non-blocking sub-agent spawn with handle + `agent_poll` | Proposed |
| [0027](0027-ask-user-interactive-prompt.md) | `ask_user` tool — model-driven user decision prompt | Proposed |
